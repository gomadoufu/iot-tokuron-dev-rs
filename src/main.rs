use core::pin::pin;
use core::slice;
use core::time::Duration;
use std::mem;

use embassy_futures::select::{select, Either};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::Delay;
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::mqtt::client::*;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::timer::{EspAsyncTimer, EspTaskTimerService, EspTimerService};
use esp_idf_svc::tls::X509;
use esp_idf_svc::wifi::*;

use esp_idf_svc::hal::{
    gpio::{InterruptType, PinDriver, Pull},
    task::notification::Notification,
};
use mpu6886::Mpu6886;
use std::num::NonZeroU32;

use log::*;

use anyhow::Result;

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_password: &'static str,
    #[default("")]
    aws_iot_endpoint: &'static str,
    #[default("")]
    aws_iot_client_id: &'static str,
    #[default("")]
    aws_iot_topic: &'static str,
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();

    // Configures the button
    let mut button = PinDriver::input(peripherals.pins.gpio42).unwrap();
    button.set_pull(Pull::Up).unwrap();
    button.set_interrupt_type(InterruptType::PosEdge).unwrap();

    // Configures the notification
    let notification = Notification::new();
    let notifier = notification.notifier();

    // Safety: make sure the `Notification` object is not dropped while the subscription is active
    unsafe {
        button
            .subscribe(move || {
                notifier.notify_and_yield(NonZeroU32::new(1).unwrap());
            })
            .unwrap();
    }
    info!("sensor initialized");

    // 1. Instanciate the SDA and SCL pins, correct pins are in the training material.
    let sda = peripherals.pins.gpio13;
    let scl = peripherals.pins.gpio15;
    // 2. Instanciate the i2c peripheral
    let config = I2cConfig::new().baudrate(400.kHz().into());
    let i2c = I2cDriver::new(peripherals.i2c0, sda, scl, &config).unwrap();
    info!("I2C initialized");

    let mut delay = Delay::default();
    let mut mpu = Mpu6886::new(i2c);

    mpu.init(&mut delay).unwrap();
    info!("sensor initialized");

    let app_config = CONFIG;
    info!("WIFI SSID = {}", app_config.wifi_ssid);
    info!("WIFI PASS = {}", app_config.wifi_password);
    info!("AWS IoT Endpoint = {}", app_config.aws_iot_endpoint);
    info!("AWS IoT Client ID = {}", app_config.aws_iot_client_id);
    info!("AWS IoT Topic = {}", app_config.aws_iot_topic);

    let sys_loop = EspSystemEventLoop::take().unwrap();
    let timer_service = EspTimerService::new().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();

    info!("ESP IDF SVC initialized");

    let mut buzzer = PinDriver::output(peripherals.pins.gpio2).unwrap();
    buzzer.set_low().unwrap();
    buzzer.set_high().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    buzzer.set_low().unwrap();

    esp_idf_svc::hal::task::block_on(async {
        let _wifi = wifi_create(
            peripherals.modem,
            &app_config,
            &sys_loop,
            &timer_service,
            &nvs,
        )
        .await?;
        info!("Wifi created");

        let server_cert =
            convert_certificate(include_bytes!("../certificates/AmazonRootCA1.pem").to_vec());
        let client_cert = convert_certificate(
            include_bytes!("../certificates/sender-certificate.pem.crt").to_vec(),
        );
        let private_key =
            convert_certificate(include_bytes!("../certificates/sender-private.pem.key").to_vec());

        let (mut client, mut conn) = mqtt_create(
            app_config.aws_iot_endpoint,
            app_config.aws_iot_client_id,
            server_cert,
            client_cert,
            private_key,
        )?;
        info!("MQTT client created");

        let mut timer = timer_service.timer_async()?;
        run(
            &mut mpu,
            &mut client,
            &mut conn,
            &mut timer,
            app_config.aws_iot_topic,
        )
        .await
    })
    .unwrap();
}

async fn run(
    mpu: &mut Mpu6886<I2cDriver<'_>>,
    client: &mut EspAsyncMqttClient,
    connection: &mut EspAsyncMqttConnection,
    timer: &mut EspAsyncTimer,
    topic: &str,
) -> Result<(), EspError> {
    info!("About to start the MQTT client");

    let res = select(
        // Need to immediately start pumping the connection for messages, or else subscribe() and publish() below will not work
        // Note that when using the alternative structure and the alternative constructor - `EspMqttClient::new_cb` - you don't need to
        // spawn a new thread, as the messages will be pumped with a backpressure into the callback you provide.
        // Yet, you still need to efficiently process each message in the callback without blocking for too long.
        //
        // Note also that if you go to http://tools.emqx.io/ and then connect and send a message to topic
        // "esp-mqtt-demo", the client configured here should receive it.
        pin!(async move {
            info!("MQTT Listening for messages");

            while let Ok(event) = connection.next().await {
                info!("[Queue] Event: {}", event.payload());
            }

            info!("Connection closed");

            Ok(())
        }),
        pin!(async move {
            // Using `pin!` is optional, but it optimizes the memory size of the Futures
            loop {
                if let Err(e) = client.subscribe(topic, QoS::AtMostOnce).await {
                    error!("Failed to subscribe to topic \"{topic}\": {e}, retrying...");

                    // Re-try in 0.5s
                    timer.after(Duration::from_millis(500)).await?;

                    continue;
                }

                info!("Subscribed to topic \"{topic}\"");

                // Just to give a chance of our connection to get even the first published message
                timer.after(Duration::from_millis(500)).await?;

                //main loop
                loop {
                    // get gyro data, scaled with sensitivity
                    let gyro = mpu.get_gyro().unwrap();
                    println!("gyro: {:?}", gyro);

                    // get accelerometer data, scaled with sensitivity
                    let acc = mpu.get_acc().unwrap();
                    println!("acc: {:?}", acc);
                    std::thread::sleep(std::time::Duration::from_secs(1));

                    let payload = format!("{{\"gyro\": {:?}, \"acc\": {:?}}}", gyro, acc);

                    client
                        .publish(topic, QoS::AtMostOnce, false, payload.as_bytes())
                        .await?;

                    info!("Published \"{payload}\" to topic \"{topic}\"");

                    let sleep_secs = 2;

                    info!("Now sleeping for {sleep_secs}s...");
                    timer.after(Duration::from_secs(sleep_secs)).await?;
                }
            }
        }),
    )
    .await;

    match res {
        Either::First(res) => res,
        Either::Second(res) => res,
    }
}

fn mqtt_create(
    url: &str,
    client_id: &str,
    server_cert: X509<'static>,
    client_cert: X509<'static>,
    private_key: X509<'static>,
) -> Result<(EspAsyncMqttClient, EspAsyncMqttConnection), EspError> {
    let (mqtt_client, mqtt_conn) = EspAsyncMqttClient::new(
        url,
        &MqttClientConfiguration {
            client_id: Some(client_id),
            crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
            server_certificate: Some(server_cert),
            client_certificate: Some(client_cert),
            private_key: Some(private_key),
            ..Default::default()
        },
    )?;

    Ok((mqtt_client, mqtt_conn))
}

async fn wifi_create(
    modem: Modem,
    app_config: &Config,
    sys_loop: &EspSystemEventLoop,
    timer_service: &EspTaskTimerService,
    nvs: &EspDefaultNvsPartition,
) -> Result<EspWifi<'static>, EspError> {
    let mut esp_wifi = EspWifi::new(modem, sys_loop.clone(), Some(nvs.clone()))?;
    let mut wifi = AsyncWifi::wrap(&mut esp_wifi, sys_loop.clone(), timer_service.clone())?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: app_config.wifi_ssid.try_into().unwrap(),
        password: app_config.wifi_password.try_into().unwrap(),
        ..Default::default()
    }))?;

    wifi.start().await?;
    info!("Wifi started");

    wifi.connect().await?;
    info!("Wifi connected");

    wifi.wait_netif_up().await?;
    info!("Wifi netif up");

    Ok(esp_wifi)
}

fn convert_certificate(mut certificate_bytes: Vec<u8>) -> X509<'static> {
    // append NUL
    certificate_bytes.push(0);

    // convert the certificate
    let certificate_slice: &[u8] = unsafe {
        let ptr: *const u8 = certificate_bytes.as_ptr();
        let len: usize = certificate_bytes.len();
        mem::forget(certificate_bytes);

        slice::from_raw_parts(ptr, len)
    };

    // return the certificate file in the correct format
    X509::pem_until_nul(certificate_slice)
}
