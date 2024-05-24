use embedded_hal::i2c::I2c;

pub mod regs;
use regs::*;

const ADDR: u8 = 0x51;

pub struct AP33772<I2C> {
    i2c: I2C,
    pub status: Status,
    pub pdos: [Option<PDO>; 7],
}

impl<I2C: I2c> AP33772<I2C> {
    pub fn new(usb_dev: I2C) -> Self {
        Self {
            i2c: usb_dev,
            pdos: [None, None, None, None, None, None, None],
            status: Status(0),
        }
    }

    pub fn update(&mut self) -> Result<Status, I2C::Error> {
        self.status.0 = self.read_status()?;
        if self.status.ready() && self.status.newpdos() {
            self.read_pdos()?;
        }
        Ok(Status(self.status.0))
    }

    fn read_buf<const N: usize>(&mut self, wbuf: &[u8]) -> Result<[u8; N], I2C::Error> {
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

    pub fn read_irqmask(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x1e], &mut buf)?;
        Ok(buf[0])
    }

    pub fn write_irqmask(&mut self, mask: u8) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[0x1e, mask])
    }

    pub fn read_npdos(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0];
        self.i2c.write_read(ADDR, &[0x1c], &mut buf)?;
        Ok(buf[0])
    }

    fn read_status(&mut self) -> Result<u8, I2C::Error> {
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

    pub fn write_ocpthr(&mut self, thr: u16) -> Result<(), I2C::Error> {
        let val: u8 = (thr / 50).try_into().unwrap();
        self.i2c.write(ADDR, &[0x23, val])
    }

    pub fn write_otpthr(&mut self, thr: u8) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[0x24, thr])
    }

    pub fn write_drthr(&mut self, thr: u8) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[0x25, thr])
    }

    pub fn read_thr(&mut self) -> Result<[u8; 3], I2C::Error> {
        // unclear why read_buf does not work here
        let mut buf: [u8; 3] = [0, 0, 0];
        self.i2c.write_read(ADDR, &[0x23], &mut buf[0..1])?;
        self.i2c.write_read(ADDR, &[0x24], &mut buf[1..2])?;
        self.i2c.write_read(ADDR, &[0x25], &mut buf[2..3])?;
        Ok(buf)
    }

    pub fn write_tr(&mut self, tr: [u8; 8]) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &tr)
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
