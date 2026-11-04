//! The `example.derived` package, generated with `prost_reflect=linked`: its messages hold those of
//! the base crate, which registers its own descriptors.

pub mod example {
    pub mod derived {
        include!("../gen/example/derived/example.derived.rs");
    }
}

#[cfg(test)]
mod tests {
    use linked_reflection_base::example::base::{base::Inner, Base};
    use prost_reflect::{DescriptorPool, DynamicMessage, ReflectMessage, Value};

    use super::example::derived::{self, Derived};

    /// A message of this crate goes through text format along with those of the base crate,
    /// including a nested message and a well-known type.
    #[test]
    fn round_trips_through_text_format() {
        let text = r#"base { name: "a" at { seconds: 1700000000 } inner { count: 3 } }
            by_key { key: "k" value { count: 4 } }"#;
        let message =
            DynamicMessage::parse_text_format(Derived::default().descriptor(), text).unwrap();
        let derived: Derived = message.transcode_to().unwrap();

        let base: Base = derived.base.clone().unwrap();
        assert_eq!(base.name, "a");
        assert_eq!(base.at.unwrap().seconds, 1_700_000_000);
        assert_eq!(base.inner, Some(Inner { count: 3 }));
        assert_eq!(derived.by_key["k"].count, 4);
        assert_eq!(
            derived.transcode_to_dynamic().to_text_format(),
            message.to_text_format()
        );
    }

    /// The pool keeps the extensions set in options: example.options, which defines only an
    /// extension, is registered by the base crate.
    #[test]
    fn keeps_the_extensions_of_options() {
        derived::register_descriptors();
        let pool = DescriptorPool::global();
        let label = pool.get_extension_by_name("example.options.label").unwrap();
        let name = Base::default()
            .descriptor()
            .get_field_by_name("name")
            .unwrap();
        assert_eq!(
            name.options().get_extension(&label).as_ref(),
            &Value::String("Name".to_owned())
        );
    }

    /// Each file is registered once, whichever crate registers first.
    #[test]
    fn registers_each_file_once() {
        Inner::default().descriptor();
        derived::register_descriptors();
        let names: Vec<_> = DescriptorPool::global()
            .files()
            .map(|file| file.name().to_owned())
            .collect();
        for file in [
            "example/base/base.proto",
            "example/derived/derived.proto",
            "example/options/options.proto",
        ] {
            assert_eq!(
                names.iter().filter(|name| *name == file).count(),
                1,
                "{file} in {names:?}"
            );
        }
    }
}
