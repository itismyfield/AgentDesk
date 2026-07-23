use super::CompletedKind;

macro_rules! define_derived_status {
    (
        $(
            $(#[$variant_attribute:meta])*
            $variant:ident
            $(($($tuple_type:ty),* $(,)?))?
            $({$($field:ident: $field_type:ty),* $(,)?})?
            => terminal $terminal:expr;
            samples [$($sample:expr),+ $(,)?];
        )+
    ) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        pub(in crate::services::discord::placeholder_live_events) enum DerivedStatus {
            $(
                $(#[$variant_attribute])*
                $variant
                $(($($tuple_type),*))?
                $({$($field: $field_type),*})?,
            )+
        }

        #[cfg(test)]
        impl DerivedStatus {
            /// The enum declaration, classifier expectation, and representative
            /// samples share this macro invocation. A new variant therefore cannot
            /// compile without declaring its contract-test samples here.
            pub(in crate::services::discord::placeholder_live_events) fn panel_shape_test_variants() -> Vec<(Self, bool)> {
                let mut variants = Vec::new();
                $(
                    variants.extend([
                        $(($sample, $terminal)),+
                    ]);
                )+
                variants
            }
        }
    };
}

define_derived_status! {
    #[default]
    Running => terminal false;
    samples [DerivedStatus::Running];

    MonitorWait => terminal false;
    samples [DerivedStatus::MonitorWait];

    ScheduleWakeup(Option<u64>) => terminal false;
    samples [
        DerivedStatus::ScheduleWakeup(Some(30)),
        DerivedStatus::ScheduleWakeup(None),
    ];

    Completed { kind: CompletedKind } => terminal true;
    samples [
        DerivedStatus::Completed {
            kind: CompletedKind::Background,
        },
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground,
        },
    ];

    ToolRunning {
        name: String,
        summary: Option<String>,
    } => terminal false;
    samples [DerivedStatus::ToolRunning {
        name: "Bash".to_string(),
        summary: None,
    }];

    SubagentRunning { desc: String } => terminal false;
    samples [DerivedStatus::SubagentRunning {
        desc: "review".to_string(),
    }];

    WorkflowRunning { label: String } => terminal false;
    samples [DerivedStatus::WorkflowRunning {
        label: "CI".to_string(),
    }];
}
