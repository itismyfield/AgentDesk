pub(crate) const COUNT: u32 = 2;

pub(super) struct Thing {
    pub(super) size: u32,
}

pub(super) fn thing() -> Thing {
    Thing { size: COUNT }
}
