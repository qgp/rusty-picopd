#![no_std]
#![no_main]

use core::cell::RefCell;
use core::cmp;
use defmt::*;
use {defmt_rtt as _, panic_probe as _};

use embedded_hal_bus::i2c as bus_i2c;

use embassy_executor::Spawner;
use embassy_futures::join;
use embassy_rp::{bind_interrupts, gpio, i2c, peripherals, usb};
use embassy_time::Timer;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<peripherals::USB>;
    I2C0_IRQ => i2c::InterruptHandler<peripherals::I2C0>;
});

use rusty_picopd::ap33772::regs::*;
use rusty_picopd::ap33772::*;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let pwr_en = gpio::Output::new(p.PIN_23, gpio::Level::Low);
    let pwr_en_rc = RefCell::new(pwr_en);
    let pdc_irq = gpio::Input::new(p.PIN_24, gpio::Pull::None);
    let mut led = gpio::Output::new(p.PIN_25, gpio::Level::Low);

    let i2c = i2c::I2c::new_async(p.I2C0, p.PIN_1, p.PIN_0, Irqs, i2c::Config::default());
    // let i2c = i2c::I2c::new_blocking(p.I2C0, p.PIN_1, p.PIN_0, i2c::Config::default());
    let i2c_ref_cell = RefCell::new(i2c);

    let mut pdc = AP33772::new(bus_i2c::RefCellDevice::new(&i2c_ref_cell));

    // initialisation
    Timer::after_millis(10).await;
    let _ = pdc.read_pdos();
    let _ = pdc.write_tr([0x10, 0x27, 0x41, 0x10, 0x88, 0x07, 0xce, 0x03]);
    let _ = pdc.write_irqmask(0xf7);
    let _ = pdc.write_ocpthr(100);
    let _ = pdc.write_otpthr(20);
    let _ = pdc.write_drthr(80);

    // choose profile
    let v_nom = 4400;
    let v_min = 3300;
    let v_max = 5000;
    let i_nom = 1000;
    let i_min = 1000;
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
                        pdo_sel = Some(pdo);
                        ipdo_sel = Some(i);
                    }
                    (PDO::Programmable(_), Some(PDO::Fixed(_))) => {
                        info!("selecting");
                        pdo_sel = Some(pdo);
                        ipdo_sel = Some(i);
                    }
                    (PDO::Fixed(_), Some(PDO::Fixed(pdo_old))) => {
                        if pdo.imax() > pdo_old.imax() {
                            info!("selecting");
                            pdo_sel = Some(pdo);
                            ipdo_sel = Some(i);
                        }
                    }
                    (PDO::Programmable(_), Some(PDO::Programmable(pdo_old))) => {
                        if pdo.imax() > pdo_old.imax() {
                            info!("selecting");
                            pdo_sel = Some(pdo);
                            ipdo_sel = Some(i);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // request profile
    match (pdo_sel, ipdo_sel) {
        (Some(PDO::Programmable(pdo)), Some(ipdo)) => {
            let mut ardo = ARDO(0);
            ardo.pos((ipdo + 1).try_into().unwrap());
            let v_set = cmp::max(cmp::min(v_nom, v_max), v_min);
            let i_set = cmp::min(i_nom, pdo.imax() * 50);
            ardo.volt(v_set / 20);
            ardo.i(i_set / 50);
            let _ = pdc.write_rdo(&RDO::ARDO(ardo));
        }
        (Some(PDO::Fixed(pdo)), Some(ipdo)) => {
            let mut frdo = FixedRDO(0);
            frdo.pos((ipdo + 1).try_into().unwrap());
            let i_set = cmp::min(i_nom, pdo.imax() * 10);
            frdo.i(i_set / 10);
            frdo.imax(i_set / 10);
            let _ = pdc.write_rdo(&RDO::FixedRDO(frdo));
        }
        _ => {}
    }

    // enable power if negotiation successful
    // TODO: wait for interrupt or timeout
    // futures::select_biased!(_ = pdc_irq.wait_for_high() => {}, _ = Timer::after_millis(100) => {});
    let irq_state = pdc_irq.is_high();
    let _ = pdc.update();
    info!("Status: 0b{:08b} - {}", pdc.status.0, irq_state);
    if pdc.status.ready() && pdc.status.success() {
        info!("Enabling output");
        let mut pwr_en = pwr_en_rc.borrow_mut();
        pwr_en.set_high();
    }

    let update_fut = async {
        // pdc_irq.wait_for_high().await;
        // info!("Updating on interrupt");
        // pdc.update();
        Timer::after_secs(1).await;
    };

    // monitor
    let write_fut = async {
        loop {
            let temp = pdc.read_temp().unwrap();
            let volt = pdc.read_voltage().unwrap();
            let curr = pdc.read_current().unwrap();
            let irq_trgd = pdc_irq.is_high();
            pdc.update().unwrap();

            info!(
                "irq: {}, status: b'{:08b}, volt: {} mV, curr: {} mA, temp: {} degC",
                irq_trgd, pdc.status.0, volt, curr, temp,
            );
            if pdc.status.ovp() || pdc.status.ocp() || pdc.status.otp() {
                info!("Disable output!");
                let mut pwr_en = pwr_en_rc.borrow_mut();
                pwr_en.set_low();
            }
            if pdc.status.newpdos() {
                for (i, pdo_opt) in pdc.pdos.iter().enumerate() {
                    if let Some(pdo) = pdo_opt {
                        info!(
                            "pdo[{}]: {} - {} mV, {} mA",
                            i + 1,
                            pdo.vmin(),
                            pdo.vmax(),
                            pdo.imax(),
                        );
                    }
                }
            }
            Timer::after_secs(5).await;
        }
    };

    let blink_fut = async {
        let mut delay_high;
        let mut delay_low;
        loop {
            {
                let pwr_en = pwr_en_rc.borrow();
                delay_high = if pwr_en.is_set_high() { 1000 } else { 100 };
                delay_low = if pwr_en.is_set_high() { 100 } else { 1000 };
            }

            led.set_high();
            Timer::after_millis(delay_high).await;

            led.set_low();
            Timer::after_millis(delay_low).await;
        }
    };

    join::join3(write_fut, update_fut, blink_fut).await;
}
