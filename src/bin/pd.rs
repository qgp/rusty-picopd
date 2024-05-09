#![no_std]
#![no_main]
#![allow(unused)]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::{bind_interrupts, gpio, i2c, usb};
use embassy_rp::peripherals::I2C0;
use embassy_time::Timer;
use embedded_hal_async::i2c::I2c;
use embassy_rp::peripherals::USB;
// use embassy_rp::usb::{Driver, Instance, InterruptHandler};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, Config};
use gpio::{Level, Output};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    I2C0_IRQ => i2c::InterruptHandler<I2C0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut led = Output::new(p.PIN_25, Level::Low);
    spawner.spawn(blink_led(led)).unwrap();

    let driver = usb::Driver::new(p.USB, Irqs);
    let mut config = Config::new(0x1556, 0xcafe);
    config.manufacturer = Some("qgp.io");
    config.product = Some("picoALPIDE");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    let mut config_descriptor = [0; 256];
    let mut device_descriptor = [0; 32];
    let mut bos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut state = State::new();
    let mut builder = Builder::new(
        driver,
        config,
        &mut device_descriptor,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [], // no msos descriptors
        &mut control_buf,
    );
    let mut class = CdcAcmClass::new(&mut builder, &mut state, 64);
    let mut usb = builder.build();
    let usb_fut = usb.run();

    let echo_fut = async {
        loop {
            class.wait_connection().await;
            info!("Connected");
            let _ = echo(&mut class).await;
            info!("Disconnected");
        }
    };

    join(usb_fut, echo_fut).await;

    const ADDR: u8 = 0x51;
    let (sda, scl) = (p.PIN_0, p.PIN_1);
    let mut i2c = i2c::I2c::new_async(p.I2C0, scl, sda, Irqs, i2c::Config::default());
    let mut status = [0];
    let status = i2c.write_read(ADDR, &[0x1d], &mut status).await.unwrap();

}

struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => defmt::panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}

async fn echo<'d, T: usb::Instance + 'd>(class: &mut CdcAcmClass<'d, usb::Driver<'d, T>>) -> Result<(), Disconnected> {
    let mut buf = [0; 64];
    loop {
        let n = class.read_packet(&mut buf).await?;
        let data = &buf[..n];
        info!("data: {:x}", data);
        class.write_packet(data).await?;
    }
}

#[embassy_executor::task]
async fn blink_led(mut led: gpio::Output<'static, impl gpio::Pin + 'static>) {
    loop {
        info!("led on!");
        led.set_high();
        Timer::after_secs(1).await;

        info!("led off!");
        led.set_low();
        Timer::after_secs(1).await;
    }
}
