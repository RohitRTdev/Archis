use alloc::string::String;
use crate::loader::LoadedImage;

struct DriverOps {
    start: Option<fn()>,
    read: Option<fn()>,
    write: Option<fn()>,
    stop: Option<fn()>
}

//struct Driver {
//    base_name: String,
//    image: LoadedImage   
//    driver_ops:  
//}

pub struct Device {
    display_name: String,

}

fn load_driver() {

}

pub fn init() {

}