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

        impl DerivedStatus {
            pub(in crate::services::discord::placeholder_live_events) fn is_terminal(&self) -> bool {
                match self {
                    $(Self::$variant
                        $(( $(define_derived_status!(@ignore $tuple_type)),* ))?
                        $({ $($field: _),* })? => $terminal,)+
                }
            }

            /// Every variant supplies samples; tests assert an independent,
            /// exhaustive expectation rather than trusting terminal metadata.
            #[cfg(test)]
            pub(in crate::services::discord::placeholder_live_events) fn panel_shape_test_variants() -> Vec<Self> {
                let mut variants = Vec::new();
                $(
                    variants.extend([
                        $($sample),+
                    ]);
                )+
                variants
            }
        }
    };
    (@ignore $tuple_type:ty) => { _ };
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
