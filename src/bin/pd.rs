#![no_std]
#![no_main]
#![allow(unused)]

use defmt::*;
use {defmt_rtt as _, panic_probe as _};

use core::cell::RefCell;

use embedded_hal::i2c::I2c as I2c_block;
use embedded_hal_async::i2c::I2c as I2c_async;
use embedded_hal_bus::i2c as bus_i2c;

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::{bind_interrupts, gpio, i2c, peripherals, usb};
use embassy_time::Timer;
use embassy_usb::{Builder, Config};
use embassy_usb::driver::EndpointError;
use embassy_usb::class::cdc_acm;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<peripherals::USB>;
    I2C0_IRQ => i2c::InterruptHandler<peripherals::I2C0>;
});

const ADDR: u8 = 0x51;
struct AP33772<I2C> {
    i2c: I2C,
}

impl<I2C: I2c_block> AP33772<I2C> {
    pub fn new(usb_dev: I2C) -> Self {
        Self {
            i2c : usb_dev,
        }
    }

    pub fn read_pdos(&mut self) -> Result<[u8; 28], I2C::Error> {
        let mut buf = [0; 28];
        self.i2c.write_read(ADDR, &[0x0], &mut buf)?;
        Ok(buf)
    }

    pub fn read_npdos(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x1c], &mut buf)?;
        Ok(buf[0])
    }

    pub fn read_status(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x1d], &mut buf)?;
        Ok(buf[0])
    }

    pub fn read_voltage(&mut self) -> Result<u16, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x20], &mut buf)?;
        Ok(buf[0] as u16 * 80)
    }

    pub fn read_temp(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x22], &mut buf)?;
        Ok(buf[0])
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut led = gpio::Output::new(p.PIN_25, gpio::Level::Low);
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
    let mut state = cdc_acm::State::new();
    let mut builder = Builder::new(
        driver,
        config,
        &mut device_descriptor,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [], // no msos descriptors
        &mut control_buf,
    );
    let mut class = cdc_acm::CdcAcmClass::new(&mut builder, &mut state, 64);
    let mut usb = builder.build();
    let usb_fut = usb.run();

    let mut i2c = i2c::I2c::new_async(p.I2C0, p.PIN_1, p.PIN_0, Irqs, i2c::Config::default());
    let i2c_ref_cell = RefCell::new(i2c);

    let mut i2c_dev = bus_i2c::RefCellDevice::new(&i2c_ref_cell);
    let mut status = [0];
    let status = i2c_dev.write_read(ADDR, &[0x1d], &mut status).unwrap();

    let mut pdc = AP33772::new(bus_i2c::RefCellDevice::new(&i2c_ref_cell));
    pdc.read_temp();

    // let echo_fut = async {
    //     loop {
    //         class.wait_connection().await;
    //         info!("Connected");
    //         let _ = echo(&mut class).await;
    //         info!("Disconnected");
    //     }
    // };

    let write_fut = async {
        let mut buf = [0; 64];
        let mut sbuf = itoa::Buffer::new();
        loop {
            // reading seems required to avoid stalling miniterm
            let n = class.read_packet(&mut buf).await;
            let temp = pdc.read_voltage().unwrap();
            class.write_packet(sbuf.format(temp).as_bytes()).await;
            class.write_packet(b"\n").await;
            Timer::after_secs(1).await;
        }
    };

    join(usb_fut, write_fut).await;
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

async fn echo<'d, T: usb::Instance + 'd>(class: &mut cdc_acm::CdcAcmClass<'d, usb::Driver<'d, T>>) -> Result<(), Disconnected> {
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
