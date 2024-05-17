#![no_std]
#![no_main]
#![allow(unused)]

use core::cell::RefCell;
use defmt::*;
use {defmt_rtt as _, panic_probe as _};

use bitfield::{bitfield, bitfield_bitrange, bitfield_fields};
use bitvec as bv;
use bitvec::prelude::*;

use embedded_hal::i2c::I2c as I2c_block;
use embedded_hal_async::i2c::I2c as I2c_async;
use embedded_hal_bus::i2c as bus_i2c;

use embassy_executor::Spawner;
use embassy_futures::join;
use embassy_rp::{bind_interrupts, gpio, i2c, peripherals, usb};
use embassy_time::Timer;
use embassy_usb::class::cdc_acm;
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, Config};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<peripherals::USB>;
    I2C0_IRQ => i2c::InterruptHandler<peripherals::I2C0>;
});

const ADDR: u8 = 0x51;

bitfield!{
    pub struct Status(u8);
    impl Debug;
    derating, _: 7;
    otp, _: 6;
    ocp, _: 5;
    ovp, _: 4;
    newpdo, _: 2;
    success, _: 1;
    ready, _: 0;
}

bitfield! {
    pub struct FixedPDO(u32);
    impl Debug;
    vmax, _: 19, 10;
    imax, _: 9, 0;
}

bitfield! {
    pub struct APDO(u32);
    impl Debug;
    vmin, _: 24, 17;
    vmax, _: 15, 8;
    imax, _: 6, 0;
}

struct AP33772<I2C> {
    i2c: I2C,
}

impl<I2C: I2c_block> AP33772<I2C> {
    pub fn new(usb_dev: I2C) -> Self {
        Self { i2c: usb_dev }
    }

    pub fn read_buf<const N: usize>(&mut self, wbuf: &[u8]) -> Result<[u8; N], I2C::Error> {
        let mut buf = [0; N];
        self.i2c.write_read(ADDR, wbuf, &mut buf)?;
        Ok(buf)
    }

    pub fn read_pdos(&mut self) -> Result<[u32; 7], I2C::Error> {
        let buf: [u8; 28] = self.read_buf(&[0x0])?;
        let mut pdos = [0u32; 7];
        for i in 0..7 {
            let pdo: &[u8; 4] = &buf[4 * i..4 * (i + 1)].try_into().unwrap();
            pdos[i] = u32::from_le_bytes(*pdo);
        }
        Ok(pdos)
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

    pub fn read_current(&mut self) -> Result<u16, I2C::Error> {
        let buf = self.read_buf::<1>(&[0x21])?;
        Ok(buf[0] as u16 * 24)
    }

    pub fn read_temp(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x22], &mut buf)?;
        Ok(buf[0])
    }

    pub fn write_rdo(&mut self) -> Result<(), I2C::Error> {
        let mut buf = bv::bitarr![u8, bv::prelude::Lsb0; 0; 32];
        // fixed PDO
        buf[31..=31].store(0); // reserved
        buf[28..=30].store(0); // object position
        buf[20..=27].store(0); // reserved
        buf[10..=19].store(100); // operating current
        buf[0..=9].store(100); // max operating current
        self.i2c.write(ADDR, buf.as_raw_slice())
    }

    pub fn reset(&mut self) -> Result<(), I2C::Error> {
        let buf = [0x30, 0, 0, 0, 0];
        self.i2c.write(ADDR, &buf)
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
    config.product = Some("picoPD");
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
    // let mut i2c = i2c::I2c::new_blocking(p.I2C0, p.PIN_1, p.PIN_0, i2c::Config::default());
    let i2c_ref_cell = RefCell::new(i2c);

    let mut i2c_dev = bus_i2c::RefCellDevice::new(&i2c_ref_cell);
    let mut pdc = AP33772::new(bus_i2c::RefCellDevice::new(&i2c_ref_cell));
    Timer::after_secs(1).await;
    let status_boot = pdc.read_status().unwrap();

    // let echo_fut = async {
    //     loop {
    //         class.wait_connection().await;
    //         info!("Connected");
    //         let _ = echo(&mut class).await;
    //         info!("Disconnected");
    //     }
    // };

    let (mut sender, mut receiver) = class.split();
    let read_fut = async {
        let mut buf = [0; 64];
        loop {
            let n = receiver.read_packet(&mut buf).await;
        }
    };

    let write_fut = async {
        let mut sbuf = itoa::Buffer::new();
        loop {
            let status = Status(pdc.read_status().unwrap());
            let temp = pdc.read_temp().unwrap();
            let volt = pdc.read_voltage().unwrap();
            let curr = pdc.read_current().unwrap();
            let npdos = pdc.read_npdos().unwrap();
            let pdos = pdc.read_pdos().unwrap();
            info!(
                "status: b'{:08b}/{:08b}, volt: {} mV, curr: {} mA, temp: {} degC, npdos: {}",
                status_boot, status.0, volt, curr, temp, npdos
            );
            // sender.write_packet(sbuf.format(volt).as_bytes()).await;
            // sender.write_packet(b"; ").await;
            // sender.write_packet(sbuf.format(curr).as_bytes()).await;
            // sender.write_packet(b"; ").await;
            // sender.write_packet(sbuf.format(temp).as_bytes()).await;
            // sender.write_packet(b"; ").await;
            // sender.write_packet(sbuf.format(npdos).as_bytes()).await;
            // sender.write_packet(b"\n").await;
            for i in 0..npdos as usize {
                if pdos[i] & 0xc000_0000 == 0 {
                    let fpdo = FixedPDO(pdos[i]);
                    info!(
                        "pdo[{}]: 0x{:08x} -> fixed: {} mV, {} mA",
                        i,
                        pdos[i],
                        fpdo.vmax() * 50,
                        fpdo.imax() * 10,
                    );
                } else if pdos[i] & 0xf000_0000 == 0xc000_0000 {
                    info!(
                        "pdo[{}]: 0x{:08x} -> PPS: {} - {} mV, {} mA",
                        i,
                        pdos[i],
                        pdos[i].view_bits::<Lsb0>()[17..=24].load::<u32>() * 100,
                        pdos[i].view_bits::<Lsb0>()[8..=15].load::<u32>() * 100,
                        pdos[i].view_bits::<Lsb0>()[0..=6].load::<u32>() * 50,
                    );
                }
                //     sender.write_packet(sbuf.format(pdos[i]).as_bytes()).await;
                //     sender.write_packet(b"\n").await;
            }
            Timer::after_secs(5).await;
        }
    };

    join::join3(usb_fut, read_fut, write_fut).await;
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

async fn echo<'d, T: usb::Instance + 'd>(
    class: &mut cdc_acm::CdcAcmClass<'d, usb::Driver<'d, T>>,
) -> Result<(), Disconnected> {
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
        // info!("led on!");
        led.set_high();
        Timer::after_secs(1).await;

        // info!("led off!");
        led.set_low();
        Timer::after_secs(1).await;
    }
}
