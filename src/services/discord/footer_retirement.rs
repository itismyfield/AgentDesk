//! Central write-side retirement gate; legacy readers remain available for recovery.

pub(in crate::services::discord) const RETIRED: bool = true;

// The rich block remains a budget input; new placeholder writes use only this anchor.
pub(in crate::services::discord) fn visible_anchor(legacy_status_block: &str) -> &str {
    if retired() {
        "⠋ 계속 처리 중"
    } else {
        legacy_status_block
    }
}

pub(in crate::services::discord) fn retired() -> bool {
    #[cfg(test)]
    if let Some(value) = RETIRED_OVERRIDE.with(std::cell::Cell::get) {
        return value;
    }
    RETIRED
}

#[cfg(test)]
thread_local! {
    static RETIRED_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(in crate::services::discord) struct LegacyForTest {
    previous: Option<bool>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
pub(in crate::services::discord) fn legacy_for_test() -> LegacyForTest {
    LegacyForTest {
        previous: RETIRED_OVERRIDE.with(|value| value.replace(Some(false))),
        _thread: std::marker::PhantomData,
    }
}

#[cfg(test)]
impl Drop for LegacyForTest {
    fn drop(&mut self) {
        RETIRED_OVERRIDE.with(|value| value.set(self.previous));
    }
}
