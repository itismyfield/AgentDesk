macro_rules! after_path_check {
    () => {
        crate::services::tui_prompt_dedupe::after_check()
    };
}

pub(super) use after_path_check;
