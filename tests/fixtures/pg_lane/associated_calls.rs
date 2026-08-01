#[cfg(test)]
mod deep {
    pub mod nested {
        pub struct C;

        impl C {
            pub fn assoc() {
                create_test_database();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::deep;

    trait Trait {
        fn f();
    }

    struct T;

    impl Trait for T {
        fn f() {
            create_test_database();
        }
    }

    fn through_multi_segment() {
        deep::nested::C::assoc();
    }

    fn through_ufcs() {
        <T as Trait>::f();
    }

    #[test]
    fn multi_segment_case() {
        through_multi_segment();
    }

    #[test]
    fn ufcs_case() {
        through_ufcs();
    }
}
