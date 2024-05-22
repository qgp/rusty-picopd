use bitfield::bitfield;

bitfield! {
    pub struct Status(u8);
    impl Debug;
    pub derating, _: 7;
    pub otp, _: 6;
    pub ocp, _: 5;
    pub ovp, _: 4;
    pub newpdos, _: 2;
    pub success, _: 1;
    pub ready, _: 0;
}

bitfield! {
    pub struct IrqMask(u8);
    impl Debug;
    pub derating, enable_derating: 7;
    pub otp, enable_otp: 6;
    pub ocp, enable_ocp: 5;
    pub ovp, enable_ovp: 4;
    pub newpdo, enable_newpdo: 2;
    pub success, enable_success: 1;
    pub ready, enable_ready: 0;
}

bitfield! {
    pub struct FixedPDO(u32);
    impl Debug;
    pub v, _: 19, 10; // LSB 50 mV
    pub imax, _: 9, 0; // LSB 10 mA
}

bitfield! {
    pub struct APDO(u32);
    impl Debug;
    pub vmax, _: 24, 17; // LSB 100 mV
    pub vmin, _: 15, 8; // LSB 100 mV
    pub imax, _: 6, 0; // LSB 50 mA
}

pub enum PDO {
    Fixed(FixedPDO),
    Programmable(APDO),
}

impl PDO {
    pub fn vmin(&self) -> u32 {
        match self {
            PDO::Fixed(pdo) => pdo.v() * 50,
            PDO::Programmable(pdo) => pdo.vmin() * 100,
        }
    }

    pub fn vmax(&self) -> u32 {
        match self {
            PDO::Fixed(pdo) => pdo.v() * 50,
            PDO::Programmable(pdo) => pdo.vmax() * 100,
        }
    }

    pub fn imax(&self) -> u32 {
        match self {
            PDO::Fixed(pdo) => pdo.imax() * 10,
            PDO::Programmable(pdo) => pdo.imax() * 50,
        }
    }

    pub fn vcomp(&self, vmin: u32, vmax: u32) -> bool {
        (vmin <= self.vmax()) && (self.vmin() <= vmax)
    }

    pub fn icomp(&self, imin: u32) -> bool {
        imin <= self.imax()
    }
}

bitfield! {
    pub struct FixedRDO(u32);
    impl Debug;
    pub _, pos: 30, 28;
    pub _, i: 19, 10; // LSB 10 mA
    pub _, imax: 9, 0; // LSB 10 mA
}

bitfield! {
    pub struct ARDO(u32);
    impl Debug;
    pub _, pos: 30, 28;
    pub _, volt: 19, 9; // LSB 20 mV
    pub _, i: 6, 0; // LSB 50 mA
}

pub enum RDO {
    FixedRDO(FixedRDO),
    ARDO(ARDO),
}

impl RDO {
    pub fn reg(&self) -> &u32 {
        match self {
            RDO::FixedRDO(v) => &v.0,
            RDO::ARDO(v) => &v.0,
        }
    }
}
