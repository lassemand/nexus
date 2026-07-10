pub mod asset;
pub mod bar;
pub mod calendar;
pub mod price;
pub mod sector;

pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/nexus.rs"));
}
