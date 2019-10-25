use super::*;
use crate::generic_heap::ObjectMeta;

const NUM_SIZE_CLASS: usize = 16;

type TSizeClasses = [SizeClass; NUM_SIZE_CLASS];

lazy_static! {
}

struct SizeClass {
    size: usize
}

pub struct Heap {

}

impl Heap {
    pub fn new() -> Self {
        unimplemented!()
    }
    pub fn allocate(&self, size: usize) -> Ptr {
        unimplemented!()
    }
    pub fn contains(&self, ptr: Ptr) -> bool {
        unimplemented!()
    }
    pub fn free(&self, ptr: Ptr) -> bool {
        unimplemented!()
    }
    pub fn meta_of(&self, ptr: Ptr) -> Option<ObjectMeta> {
        unimplemented!()
    }
    pub fn size_of(&self, ptr: Ptr) -> Option<usize> {
        unimplemented!()
    }
}