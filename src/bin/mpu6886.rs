use anyhow::Result;
use esp_idf_svc::hal::delay::Delay;
use esp_idf_svc::hal::{
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    prelude::*,
};
use log::info;
use mpu6886::*;

// Goals of this exercise:
// - Part1: Instantiate i2c peripheral
// - Part1: Implement one sensor, print sensor values
// - Part2: Implement second sensor on same bus to solve an ownership problem

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();

    // 1. Instanciate the SDA and SCL pins, correct pins are in the training material.
    let sda = peripherals.pins.gpio13;
    let scl = peripherals.pins.gpio15;
    // 2. Instanciate the i2c peripheral
    let config = I2cConfig::new().baudrate(400.kHz().into());
    let i2c = I2cDriver::new(peripherals.i2c0, sda, scl, &config)?;
    info!("I2C initialized");

    let mut delay = Delay::default();
    let mut mpu = Mpu6886::new(i2c);

    mpu.init(&mut delay).unwrap();

    loop {
        // get roll and pitch estimate
        let acc = mpu.get_acc_angles().unwrap();
        println!("r/p: {:?}", acc);

        // get sensor temp
        let temp = mpu.get_temp().unwrap();
        println!("temp: {:?}c", temp);

        // get gyro data, scaled with sensitivity
        let gyro = mpu.get_gyro().unwrap();
        println!("gyro: {:?}", gyro);

        // get accelerometer data, scaled with sensitivity
        let acc = mpu.get_acc().unwrap();
        println!("acc: {:?}", acc);
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
