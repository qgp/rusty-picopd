#![no_std]
#![no_main]
#![allow(unused)]

use core::cell::RefCell;
use core::cmp;
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

bitfield! {
    pub struct Status(u8);
    impl Debug;
    derating, _: 7;
    otp, _: 6;
    ocp, _: 5;
    ovp, _: 4;
    newpdos, _: 2;
    success, _: 1;
    ready, _: 0;
}

bitfield! {
    pub struct FixedPDO(u32);
    impl Debug;
    v, _: 19, 10; // LSB 50 mV
    imax, _: 9, 0; // LSB 10 mA
}

bitfield! {
    pub struct APDO(u32);
    impl Debug;
    vmax, _: 24, 17; // LSB 100 mV
    vmin, _: 15, 8; // LSB 100 mV
    imax, _: 6, 0; // LSB 50 mA
}

enum PDO {
    Fixed(FixedPDO),
    Programmable(APDO),
}

impl PDO {
    fn vmin(&self) -> u32 {
        match self {
            PDO::Fixed(pdo) => pdo.v() * 50,
            PDO::Programmable(pdo) => pdo.vmin() * 100,
        }
    }

    fn vmax(&self) -> u32 {
        match self {
            PDO::Fixed(pdo) => pdo.v() * 50,
            PDO::Programmable(pdo) => pdo.vmax() * 100,
        }
    }

    fn imax(&self) -> u32 {
        match self {
            PDO::Fixed(pdo) => pdo.imax() * 10,
            PDO::Programmable(pdo) => pdo.imax() * 50,
        }
    }

    fn vcomp(&self, vmin: u32, vmax: u32) -> bool {
        (vmin <= self.vmax()) && (self.vmin() <= vmax)
    }

    fn icomp(&self, imin: u32) -> bool {
        imin <= self.imax()
    }
}

bitfield! {
    pub struct FixedRDO(u32);
    impl Debug;
    _, pos: 30, 28;
    _, i: 19, 10; // LSB 10 mA
    _, imax: 9, 0; // LSB 10 mA
}

bitfield! {
    pub struct ARDO(u32);
    impl Debug;
    _, pos: 30, 28;
    _, volt: 19, 9; // LSB 20 mV
    _, i: 6, 0; // LSB 50 mA
}

enum RDO {
    FixedRDO(FixedRDO),
    ARDO(ARDO),
}

impl RDO {
    fn reg(&self) -> &u32 {
        match self {
            RDO::FixedRDO(v) => &v.0,
            RDO::ARDO(v) => &v.0,
        }
    }
}

struct AP33772<I2C> {
    i2c: I2C,
    status: Status,
    pdos: [Option<PDO>; 7],
}

impl<I2C: I2c_block> AP33772<I2C> {
    pub fn new(usb_dev: I2C) -> Self {
        Self {
            i2c: usb_dev,
            pdos: [None, None, None, None, None, None, None],
            status: Status(0),
        }
    }

    pub fn update(&mut self) -> Result<(), I2C::Error> {
        self.status.0 = self.read_status()?;
        if self.status.ready() && self.status.newpdos() {
            self.read_pdos();
        }
        Ok(())
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
            self.pdos[i] = if pdos[i] == 0x0 {
                None
            } else if pdos[i] & 0xf000_0000 == 0xc000_0000 {
                Some(PDO::Programmable(APDO(pdos[i])))
            } else if pdos[i] & 0xc000_0000 == 0x0 {
                Some(PDO::Fixed(FixedPDO(pdos[i])))
            } else {
                None
            };
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

    pub fn write_rdo(&mut self, rdo: &RDO) -> Result<(), I2C::Error> {
        let mut buf = [0u8; 5];
        buf[0] = 0x30;
        buf[1..5].copy_from_slice(&rdo.reg().to_le_bytes());
        self.i2c.write(ADDR, &buf)
    }

    pub fn reset(&mut self) -> Result<(), I2C::Error> {
        let buf = [0x30, 0, 0, 0, 0];
        self.i2c.write(ADDR, &buf)
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut pwr_en = gpio::Output::new(p.PIN_23, gpio::Level::Low);
    let mut led = gpio::Output::new(p.PIN_25, gpio::Level::Low);
    spawner.spawn(blink_led(led)).unwrap();

    let driver = usb::Driver::new(p.USB, Irqs);
    let mut config = Config::new(0x1556, 0xcafe);
    config.manufacturer = Some("qgp.io");
    config.product = Some("picoPD");
    config.serial_number = Some("12345678"); // user s/n of flash: https://docs.rs/embassy-rp/latest/embassy_rp/flash/struct.Flash.html#method.blocking_unique_id
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
    Timer::after_millis(10).await;
    let status_boot = pdc.read_status().unwrap();
    pdc.read_pdos();

    let v_nom = 4200;
    let v_min = 3300;
    let v_max = 5000;
    let i_nom = 2000;
    let i_min = 1500;
    let mut ipdo_sel: Option<usize> = None;
    let mut pdo_sel: Option<&PDO> = None;
    for (i, pdo_opt) in pdc.pdos.iter().enumerate() {
        if let Some(pdo) = pdo_opt {
            info!(
                "pdo[{}]: {} - {} mV, {} mA",
                i + 1,
                pdo.vmin(),
                pdo.vmax(),
                pdo.imax(),
            );
            if pdo.vcomp(v_min, v_max) && pdo.icomp(i_min) {
                info!("compatible");
                match (pdo, pdo_sel) {
                    (_, None) => {
                        info!("selecting");
                        pdo_sel = Some(&pdo);
                        ipdo_sel = Some(i);
                    }
                    (PDO::Programmable(_), Some(PDO::Fixed(_))) => {
                        info!("selecting");
                        pdo_sel = Some(&pdo);
                        ipdo_sel = Some(i);
                    }
                    (PDO::Fixed(_), Some(PDO::Fixed(pdo_old))) => {
                        if pdo.imax() > pdo_old.imax() {
                            info!("selecting");
                            pdo_sel = Some(&pdo);
                            ipdo_sel = Some(i);
                        }
                    }
                    (PDO::Programmable(_), Some(PDO::Programmable(pdo_old))) => {
                        if pdo.imax() > pdo_old.imax() {
                            info!("selecting");
                            pdo_sel = Some(&pdo);
                            ipdo_sel = Some(i);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    match (pdo_sel, ipdo_sel) {
        (Some(PDO::Programmable(pdo)), Some(ipdo)) => {
            let mut ardo = ARDO(0);
            ardo.pos((ipdo + 1).try_into().unwrap());
            let v_set = cmp::max(cmp::min(v_nom, v_max), v_min);
            let i_set = cmp::min(i_nom, pdo.imax() * 50);
            ardo.volt(v_set / 20);
            ardo.i(i_set / 50);
            pdc.write_rdo(&RDO::ARDO(ardo));
        }
        (Some(PDO::Fixed(pdo)), Some(ipdo)) => {
            let mut frdo = FixedRDO(0);
            frdo.pos((ipdo + 1).try_into().unwrap());
            let i_set = cmp::min(i_nom, pdo.imax() * 10);
            frdo.i(i_set / 10);
            frdo.imax(i_set / 10);
            pdc.write_rdo(&RDO::FixedRDO(frdo));
        }
        _ => {}
    }

    Timer::after_millis(100).await;
    pdc.update();
    if pdc.status.ready() && pdc.status.success() {
        info!("Enabling output");
        pwr_en.set_high();
    }

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
            for (i, pdo) in pdc.pdos.iter().enumerate() {
                match pdo {
                    Some(PDO::Fixed(fpdo)) => {
                        info!(
                            "pdo[{}]: 0x{:08x} -> fixed: {} mV, {} mA",
                            i + 1,
                            fpdo.0,
                            fpdo.v() * 50,
                            fpdo.imax() * 10,
                        );
                    }
                    Some(PDO::Programmable(apdo)) => {
                        info!(
                            "pdo[{}]: 0x{:08x} -> PPS: {} - {} mV, {} mA",
                            i + 1,
                            apdo.0,
                            apdo.vmin() * 100,
                            apdo.vmax() * 100,
                            apdo.imax() * 50,
                        );
                    }
                    _ => {}
                }
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
