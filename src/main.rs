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
    let mut pdc_irq = gpio::Input::new(p.PIN_24, gpio::Pull::None);
    let mut led = gpio::Output::new(p.PIN_25, gpio::Level::Low);

    let i2c = i2c::I2c::new_async(p.I2C0, p.PIN_1, p.PIN_0, Irqs, i2c::Config::default());
    // let i2c = i2c::I2c::new_blocking(p.I2C0, p.PIN_1, p.PIN_0, i2c::Config::default());
    let i2c_ref_cell = RefCell::new(i2c);

    let pdc = AP33772::new(bus_i2c::RefCellDevice::new(&i2c_ref_cell));
    let pdc_rc = RefCell::new(pdc);

    // initialisation
    Timer::after_millis(10).await;
    {
        let mut pdc = pdc_rc.borrow_mut();
        let _ = pdc.read_pdos();
        let _ = pdc.write_tr([0x10, 0x27, 0x41, 0x10, 0x88, 0x07, 0xce, 0x03]);
        let _ = pdc.write_irqmask(0xf7);
        let _ = pdc.write_ocpthr(200);
        let _ = pdc.write_otpthr(80);
        let _ = pdc.write_drthr(60);
    }

    // choose profile
    let v_nom = 3400;
    let v_min = 3300;
    let v_max = 5000;
    let i_nom = 1000;
    let i_min = 1000;
    let mut ipdo_sel: Option<usize> = None;
    let mut pdo_sel: Option<&PDO> = None;
    {
        let pdc = &mut pdc_rc.borrow_mut();
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
                    debug!("  compatible");
                    match (pdo, pdo_sel) {
                        // something is better than nothing
                        (_, None) => {
                            debug!("  selecting");
                            pdo_sel = Some(pdo);
                            ipdo_sel = Some(i);
                        }
                        // programmable is better than fixed
                        (PDO::Programmable(_), Some(PDO::Fixed(_))) => {
                            debug!("  selecting");
                            pdo_sel = Some(pdo);
                            ipdo_sel = Some(i);
                        }
                        // more current is better
                        (PDO::Fixed(_), Some(PDO::Fixed(pdo_old))) => {
                            if pdo.imax() > pdo_old.imax() {
                                debug!("  selecting");
                                pdo_sel = Some(pdo);
                                ipdo_sel = Some(i);
                            }
                        }
                        // more current is better
                        (PDO::Programmable(_), Some(PDO::Programmable(pdo_old))) => {
                            if pdo.imax() > pdo_old.imax() {
                                debug!("  selecting");
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
                info!("requested PPS");
            }
            (Some(PDO::Fixed(pdo)), Some(ipdo)) => {
                let mut frdo = FixedRDO(0);
                frdo.pos((ipdo + 1).try_into().unwrap());
                let i_set = cmp::min(i_nom, pdo.imax() * 10);
                frdo.i(i_set / 10);
                frdo.imax(i_set / 10);
                info!("request fixed RDO 0x{:08x}", &frdo.0);
                let _ = pdc.write_rdo(&RDO::FixedRDO(frdo));
            }
            _ => {}
        }
    }

    let control_fut = async {
        let mut init = true;
        loop {
            pdc_irq.wait_for_high().await;
            info!("Updating on interrupt");
            let status_res = pdc_rc.borrow_mut().update();
            if let Ok(status) = status_res {
                info!("Status: 0b{:08b}", status.0);
                if init {
                    pdc_rc.borrow_mut().read_pdos().ok();
                }
                if init || status.newpdos() {
                    init = false;
                    let pdos = &pdc_rc.borrow().pdos;
                    for (i, pdo_opt) in pdos.iter().enumerate() {
                        if let Some(pdo) = pdo_opt {
                            info!(
                                "pdo[{}]: {} - {} mv, {} ma",
                                i + 1,
                                pdo.vmin(),
                                pdo.vmax(),
                                pdo.imax(),
                            );
                        }
                    }
                    // TODO: update profile
                }
                if status.ovp() || status.ocp() || status.otp() {
                    info!("Disable output!");
                    let mut pwr_en = pwr_en_rc.borrow_mut();
                    pwr_en.set_low();
                    continue;
                }
                if status.ready() && status.success() {
                    info!("Enabling output");
                    let mut pwr_en = pwr_en_rc.borrow_mut();
                    pwr_en.set_high();
                }
            }
        }
    };

    let monitor_fut = async {
        loop {
            {
                let mut pdc = pdc_rc.borrow_mut();
                let temp = pdc.read_temp().unwrap();
                let volt = pdc.read_voltage().unwrap();
                let curr = pdc.read_current().unwrap();

                info!("volt: {} mV, curr: {} mA, temp: {} degC", volt, curr, temp,);
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

    join::join3(monitor_fut, control_fut, blink_fut).await;
}
