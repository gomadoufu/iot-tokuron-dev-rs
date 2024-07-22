use core::time::Duration;
use std::{mem, slice};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::Delay;
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::mqtt::client::*;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::tls::X509;
use esp_idf_svc::wifi::*;

use log::*;
use mpu6886::Mpu6886;

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
    info!("mpu6886 initialized");

    let app_config = CONFIG;
    info!("WIFI SSID = {}", app_config.wifi_ssid);
    info!("WIFI PASS = {}", app_config.wifi_password);
    info!("AWS IoT Endpoint = {}", app_config.aws_iot_endpoint);
    info!("AWS IoT Client ID = {}", app_config.aws_iot_client_id);
    info!("AWS IoT Topic = {}", app_config.aws_iot_topic);

    let sys_loop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    info!("ESP IDF SVC initialized");

    let _wifi = wifi_create(&app_config, &sys_loop, &nvs).unwrap();
    info!("Wifi created");

    let server_cert =
        convert_certificate(include_bytes!("../../certificates/AmazonRootCA1.pem").to_vec());
    let client_cert = convert_certificate(
        include_bytes!("../../certificates/sender-certificate.pem.crt").to_vec(),
    );
    let private_key =
        convert_certificate(include_bytes!("../../certificates/sender-private.pem.key").to_vec());

    let (mut client, mut conn) = mqtt_create(
        app_config.aws_iot_endpoint,
        app_config.aws_iot_client_id,
        server_cert,
        client_cert,
        private_key,
    )
    .unwrap();
    info!("MQTT client created");

    run(&mut client, &mut conn, app_config.aws_iot_topic).unwrap();
}

fn run(
    client: &mut EspMqttClient<'_>,
    connection: &mut EspMqttConnection,
    topic: &str,
) -> Result<(), EspError> {
    std::thread::scope(|s| {
        info!("About to start the MQTT client");

        // Need to immediately start pumping the connection for messages, or else subscribe() and publish() below will not work
        // Note that when using the alternative constructor - `EspMqttClient::new_cb` - you don't need to
        // spawn a new thread, as the messages will be pumped with a backpressure into the callback you provide.
        // Yet, you still need to efficiently process each message in the callback without blocking for too long.
        //
        // Note also that if you go to http://tools.emqx.io/ and then connect and send a message to topic
        // "esp-mqtt-demo", the client configured here should receive it.
        std::thread::Builder::new()
            .stack_size(6000)
            .spawn_scoped(s, move || {
                info!("MQTT Listening for messages");

                while let Ok(event) = connection.next() {
                    info!("[Queue] Event: {}", event.payload());
                }

                info!("Connection closed");
            })
            .unwrap();

        loop {
            if let Err(e) = client.subscribe(topic, QoS::AtMostOnce) {
                error!("Failed to subscribe to topic \"{topic}\": {e}, retrying...");

                // Re-try in 0.5s
                std::thread::sleep(Duration::from_millis(500));

                continue;
            }

            info!("Subscribed to topic \"{topic}\"");

            // Just to give a chance of our connection to get even the first published message
            std::thread::sleep(Duration::from_millis(500));

            let payload = "Hello from esp-mqtt-demo!";

            loop {
                client.enqueue(topic, QoS::AtMostOnce, false, payload.as_bytes())?;

                info!("Published \"{payload}\" to topic \"{topic}\"");

                let sleep_secs = 2;

                info!("Now sleeping for {sleep_secs}s...");
                std::thread::sleep(Duration::from_secs(sleep_secs));
            }
        }
    })
}

fn mqtt_create(
    url: &str,
    client_id: &str,
    server_cert: X509<'static>,
    client_cert: X509<'static>,
    private_key: X509<'static>,
) -> Result<(EspMqttClient<'static>, EspMqttConnection), EspError> {
    let (mqtt_client, mqtt_conn) = EspMqttClient::new(
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

fn wifi_create(
    app_config: &Config,
    sys_loop: &EspSystemEventLoop,
    nvs: &EspDefaultNvsPartition,
) -> Result<EspWifi<'static>, EspError> {
    let peripherals = Peripherals::take()?;

    let mut esp_wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs.clone()))?;
    let mut wifi = BlockingWifi::wrap(&mut esp_wifi, sys_loop.clone())?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: app_config.wifi_ssid.try_into().unwrap(),
        password: app_config.wifi_password.try_into().unwrap(),
        ..Default::default()
    }))?;

    wifi.start()?;
    info!("Wifi started");

    wifi.connect()?;
    info!("Wifi connected");

    wifi.wait_netif_up()?;
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
