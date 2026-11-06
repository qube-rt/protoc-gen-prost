#![doc = include_str!("../README.md")]

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    fmt,
    str::{self},
};

use once_cell::sync::Lazy;
use prost::Message;
use prost_build::Module;
use prost_types::{
    compiler::{code_generator_response::File, CodeGeneratorRequest},
    DescriptorProto, FileDescriptorProto,
};

use self::generator::{CoreProstGenerator, FileDescriptorSetGenerator, RegistrationGenerator};

mod generator;

pub use self::generator::{Error, Generator, GeneratorResultExt, Result};

/// Execute the core _Prost!_ generator from an encoded [`CodeGeneratorRequest`]
pub fn execute(raw_request: &[u8]) -> generator::Result {
    let request = CodeGeneratorRequest::decode(raw_request)?;
    let params = request.parameter().parse::<Parameters>()?;

    let registration_generator =
        if params.file_descriptor_set && params.prost_reflect == Some(ProstReflect::Linked) {
            Some(RegistrationGenerator::new(
                &request,
                &RawProtos::decode(raw_request)?,
                &params.prost.extern_path,
            ))
        } else {
            None
        };

    let module_request_set = ModuleRequestSet::new(
        request.file_to_generate,
        request.proto_file,
        raw_request,
        params.prost.default_package_filename(),
        params.prost.flat_output_dir,
    )?;

    let file_descriptor_set_generator = if params.file_descriptor_set {
        Some(if params.prost_reflect == Some(ProstReflect::Embedded) {
            // When using prost-reflect, we need to include all dependencies
            FileDescriptorSetGenerator::with_all_dependencies()
        } else {
            // Normal mode: only include files from each module
            FileDescriptorSetGenerator::new(params.generate_files_only)
        })
    } else {
        None
    };

    let mut config = params.prost.to_prost_config();

    if params.generate_files_only {
        // Prost generates every file it is given. It still needs the other files of the generated
        // packages, to derive Eq or Copy from the messages they define, so their types are mapped
        // to a path that nothing references instead, which prost does not generate. A generated
        // file can only refer to them through an extern_path of the caller's, which this keeps.
        for (_, request) in module_request_set.requests() {
            if request.output_filename().is_none() {
                continue;
            }
            for (file, to_generate) in request.files().zip(request.files_to_generate()) {
                if to_generate {
                    continue;
                }
                let types = file
                    .message_type
                    .iter()
                    .map(|message| message.name())
                    .chain(file.enum_type.iter().map(|enumeration| enumeration.name()));
                for name in types {
                    let proto_path = format!(".{}.{name}", file.package());
                    let mapped = params.prost.extern_path.iter().any(|(path, _)| {
                        proto_path == *path || proto_path.starts_with(&format!("{path}."))
                    });
                    if !mapped {
                        config.extern_path(proto_path, format!("::not_generated::{name}"));
                    }
                }
            }
        }
    }

    if params.file_descriptor_set && params.prost_reflect.is_some() {
        let mut messages = Vec::new();
        for (_, request) in module_request_set.requests() {
            for (file, to_generate) in request.files().zip(request.files_to_generate()) {
                if params.generate_files_only && !to_generate {
                    continue;
                }
                collect_message_names(file.package(), &file.message_type, 0, &mut messages);
            }
        }

        for (full_name, nesting_depth) in &messages {
            // file_descriptor_set_generator declares FILE_DESCRIPTOR_SET, and
            // registration_generator `__descriptor_pool()`, in the package-level
            // module. A message nested `nesting_depth` levels deep lives in that many
            // `pub mod <parent>` wrappers below it, so the reference needs a matching
            // number of `super::` segments -- an unqualified name only resolves
            // for top-level messages.
            let package_module = "super::".repeat(*nesting_depth);
            config
                .type_attribute(full_name, "#[derive(::prost_reflect::ReflectMessage)]")
                .type_attribute(
                    full_name,
                    match params.prost_reflect {
                        Some(ProstReflect::Embedded) => format!(
                            r#"#[prost_reflect(file_descriptor_set_bytes = "{package_module}FILE_DESCRIPTOR_SET")]"#
                        ),
                        _ => format!(
                            r#"#[prost_reflect(descriptor_pool = "{package_module}__descriptor_pool()")]"#
                        ),
                    },
                )
                .type_attribute(
                    full_name,
                    format!("#[prost_reflect(message_name = \"{}\")]", full_name),
                );
        }
    }

    let files = CoreProstGenerator::new(config)
        .chain(file_descriptor_set_generator)
        .chain(registration_generator)
        .generate(&module_request_set)?;

    Ok(files)
}

/// Recursively collect the fully-qualified names of all messages, including messages nested
/// inside other messages, together with each message's nesting depth (0 for top-level messages).
fn collect_message_names(
    package_name: &str,
    messages: &[DescriptorProto],
    nesting_depth: usize,
    out: &mut Vec<(String, usize)>,
) {
    for message in messages {
        let full_name = format!("{}.{}", package_name, message.name());
        out.push((full_name.clone(), nesting_depth));
        collect_message_names(&full_name, &message.nested_type, nesting_depth + 1, out);
    }
}

/// A set of requests to generate code for a series of modules
pub struct ModuleRequestSet {
    requests: BTreeMap<Module, ModuleRequest>,
}

impl ModuleRequestSet {
    /// Construct a new module request set from an encoded [`CodeGeneratorRequest`]
    ///
    /// [`CodeGeneratorRequest`]: prost_types::compiler::CodeGeneratorRequest
    pub fn new<I>(
        input_protos: I,
        proto_file: Vec<FileDescriptorProto>,
        raw_request: &[u8],
        default_package_filename: Option<&str>,
        flat_output_dir: bool,
    ) -> std::result::Result<Self, prost::DecodeError>
    where
        I: IntoIterator<Item = String>,
    {
        let raw_protos = RawProtos::decode(raw_request)?;

        Ok(Self::new_decoded(
            input_protos,
            proto_file,
            raw_protos,
            default_package_filename.unwrap_or("_"),
            flat_output_dir,
        ))
    }

    fn new_decoded<I>(
        input_protos: I,
        proto_file: Vec<FileDescriptorProto>,
        raw_protos: RawProtos,
        default_package_filename: &str,
        flat_output_dir: bool,
    ) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let input_protos: HashSet<_> = input_protos.into_iter().collect();

        let requests = proto_file.into_iter().zip(raw_protos.proto_file).fold(
            BTreeMap::new(),
            |mut acc, (proto, raw)| {
                let module = Module::from_protobuf_package_name(proto.package());
                let proto_filename = proto.name();
                let entry = acc.entry(module.clone()).or_insert_with(|| {
                    ModuleRequest::new(proto.package().to_owned(), module, flat_output_dir)
                });

                let to_generate = input_protos.contains(proto_filename);
                if entry.output_filename().is_none() && to_generate {
                    let filename = match proto.package() {
                        "" => default_package_filename.to_owned(),
                        package => format!("{package}.rs"),
                    };
                    entry.with_output_filename(filename);
                }

                entry.push_file_descriptor_proto(proto, raw, to_generate);
                acc
            },
        );

        Self { requests }
    }

    /// An ordered iterator of all requests
    pub fn requests(&self) -> impl Iterator<Item = (&Module, &ModuleRequest)> {
        self.requests.iter()
    }

    /// Retrieve the request for the given module
    pub fn for_module(&self, module: &Module) -> Option<&ModuleRequest> {
        self.requests.get(module)
    }

    pub fn modules(&self) -> impl Iterator<Item = &Module> {
        self.requests.keys()
    }
}

/// A code generation request for a specific module
pub struct ModuleRequest {
    proto_package_name: String,
    module: Module,
    flat_output_dir: bool,
    output_filename: Option<String>,
    files: Vec<FileDescriptorProto>,
    raw: Vec<Vec<u8>>,
    /// Whether each of `files` is in the request's `file_to_generate`
    to_generate: Vec<bool>,
}

impl ModuleRequest {
    fn new(proto_package_name: String, module: Module, flat_output_dir: bool) -> Self {
        Self {
            proto_package_name,
            module,
            flat_output_dir,
            output_filename: None,
            files: Vec::new(),
            raw: Vec::new(),
            to_generate: Vec::new(),
        }
    }

    fn with_output_filename(&mut self, filename: String) {
        self.output_filename = Some(filename);
    }

    fn push_file_descriptor_proto(
        &mut self,
        encoded: FileDescriptorProto,
        raw: Vec<u8>,
        to_generate: bool,
    ) {
        self.files.push(encoded);
        self.raw.push(raw);
        self.to_generate.push(to_generate);
    }

    /// The protobuf package name for this module
    pub fn proto_package_name(&self) -> &str {
        &self.proto_package_name
    }

    /// The output filename for this module
    pub fn output_filename(&self) -> Option<&str> {
        self.output_filename.as_deref()
    }

    pub fn output_dir(&self) -> String {
        if self.flat_output_dir {
            return String::new();
        }
        let mut output_dir = self.module.parts().collect::<Vec<_>>().join("/");
        if !output_dir.is_empty() {
            output_dir.push('/');
        }
        output_dir
    }

    pub fn output_filepath(&self) -> Option<String> {
        self.output_filename().map(|f| {
            let dir = self.output_dir();
            format!("{dir}{f}")
        })
    }

    /// An iterator of the file descriptors
    pub fn files(&self) -> impl Iterator<Item = &FileDescriptorProto> {
        self.files.iter()
    }

    /// An iterator of the encoded [`FileDescriptorProto`]s from [`files()`][Self::files()]
    pub fn raw_files(&self) -> impl Iterator<Item = &[u8]> {
        self.raw.iter().map(|b| b.as_slice())
    }

    /// Whether each of the files of [`files()`][Self::files()], in the same order, is in the
    /// request's `file_to_generate`, rather than only imported by those
    pub fn files_to_generate(&self) -> impl Iterator<Item = bool> + '_ {
        self.to_generate.iter().copied()
    }

    /// Creates a code generation file from the output
    pub(crate) fn write_to_file<F: FnOnce(&mut String)>(&self, f: F) -> Option<File> {
        self.output_filepath().map(|name| {
            let mut content = String::with_capacity(8_192);
            f(&mut content);

            File {
                name: Some(name),
                content: Some(content),
                ..Default::default()
            }
        })
    }

    /// Appends generated code to the end of the main file for this module
    ///
    /// This is generally a good way to add includes referencing the output
    /// of other plugins or to directly append to the main file.
    pub fn append_to_file<F: FnOnce(&mut String)>(&self, f: F) -> Option<File> {
        self.output_filepath().map(|name| {
            let mut content = String::new();
            f(&mut content);

            File {
                name: Some(name),
                content: Some(content),
                insertion_point: Some("module".to_owned()),
                ..Default::default()
            }
        })
    }
}

/// Parameters use to configure [`Generator`]s built into `protoc-gen-prost`
///
/// [`Generator`]: crate::Generator
#[derive(Debug, Default)]
struct Parameters {
    /// Prost parameters, used to generate [`prost_build::Config`]
    prost: ProstParameters,

    /// Whether a file descriptor set has been requested in each module
    file_descriptor_set: bool,

    /// Whether to generate the files of `file_to_generate` alone, rather than every file of their
    /// packages in the request
    generate_files_only: bool,

    /// Whether to generate prost-reflect trait implementations for the generated
    /// rust types, and how they find their descriptors
    prost_reflect: Option<ProstReflect>,
}

/// How the generated `ReflectMessage` implementations find their descriptors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProstReflect {
    /// Each module embeds the descriptors of its files and of all the files they import
    Embedded,
    /// Each module embeds the descriptors of its files only, and registers them in prost-reflect's
    /// global pool after the modules the `extern_path`s map the imported files to
    Linked,
}

/// Parameters used to configure the underlying Prost generator
#[derive(Debug, Default)]
struct ProstParameters {
    btree_map: Vec<String>,
    bytes: Vec<String>,
    boxed: Vec<String>,
    disable_comments: Vec<String>,
    skip_debug: Vec<String>,
    default_package_filename: Option<String>,
    extern_path: Vec<(String, String)>,
    type_attribute: Vec<(String, String)>,
    field_attribute: Vec<(String, String)>,
    enum_attribute: Vec<(String, String)>,
    message_attribute: Vec<(String, String)>,
    compile_well_known_types: bool,
    retain_enum_prefix: bool,
    enable_type_names: bool,
    flat_output_dir: bool,
}

impl ProstParameters {
    /// Builds a [`prost_build::Config`] from the parameters
    fn to_prost_config(&self) -> prost_build::Config {
        let mut config = prost_build::Config::new();
        config.btree_map(self.btree_map.iter());
        config.bytes(self.bytes.iter());
        for b in self.boxed.iter() {
            config.boxed(b);
        }
        config.disable_comments(self.disable_comments.iter());
        config.skip_debug(self.skip_debug.iter());

        if let Some(filename) = self.default_package_filename.as_deref() {
            config.default_package_filename(filename);
        }

        for (proto_path, rust_path) in &self.extern_path {
            config.extern_path(proto_path, rust_path);
        }
        for (proto_path, attribute) in &self.type_attribute {
            config.type_attribute(proto_path, attribute);
        }
        for (proto_path, attribute) in &self.field_attribute {
            config.field_attribute(proto_path, attribute);
        }
        for (proto_path, attribute) in &self.enum_attribute {
            config.enum_attribute(proto_path, attribute);
        }
        for (proto_path, attribute) in &self.message_attribute {
            config.message_attribute(proto_path, attribute);
        }

        if self.compile_well_known_types {
            config.compile_well_known_types();
        }
        if self.retain_enum_prefix {
            config.retain_enum_prefix();
        }
        if self.enable_type_names {
            config.enable_type_names();
        }

        config
    }

    fn default_package_filename(&self) -> Option<&str> {
        self.default_package_filename.as_deref()
    }

    fn try_handle_parameter<'a>(&mut self, param: Param<'a>) -> std::result::Result<(), Param<'a>> {
        match param {
            Param::Value {
                param: "btree_map",
                value,
            } => self.btree_map.push(value.to_string()),
            Param::Value {
                param: "bytes",
                value,
            } => self.bytes.push(value.to_string()),
            Param::Value {
                param: "boxed",
                value,
            } => self.boxed.push(value.to_string()),
            Param::Parameter {
                param: "default_package_filename",
            }
            | Param::Value {
                param: "default_package_filename",
                ..
            } => self.default_package_filename = param.value().map(|s| s.into_owned()),
            Param::Parameter {
                param: "compile_well_known_types",
            }
            | Param::Value {
                param: "compile_well_known_types",
                value: "true",
            } => self.compile_well_known_types = true,
            Param::Value {
                param: "compile_well_known_types",
                value: "false",
            } => (),
            Param::Value {
                param: "disable_comments",
                value,
            } => self.disable_comments.push(value.to_string()),
            Param::Value {
                param: "skip_debug",
                value,
            } => self.skip_debug.push(value.to_string()),
            Param::Parameter {
                param: "retain_enum_prefix",
            }
            | Param::Value {
                param: "retain_enum_prefix",
                value: "true",
            } => self.retain_enum_prefix = true,
            Param::Value {
                param: "retain_enum_prefix",
                value: "false",
            } => (),
            Param::KeyValue {
                param: "extern_path",
                key: prefix,
                value: module,
            } => self.extern_path.push((prefix.to_string(), module)),
            Param::KeyValue {
                param: "type_attribute",
                key: prefix,
                value: module,
            } => self.type_attribute.push((
                prefix.to_string(),
                module.replace(r"\,", ",").replace(r"\\", r"\"),
            )),
            Param::KeyValue {
                param: "field_attribute",
                key: prefix,
                value: module,
            } => self.field_attribute.push((
                prefix.to_string(),
                module.replace(r"\,", ",").replace(r"\\", r"\"),
            )),
            Param::KeyValue {
                param: "enum_attribute",
                key: prefix,
                value: module,
            } => self.enum_attribute.push((
                prefix.to_string(),
                module.replace(r"\,", ",").replace(r"\\", r"\"),
            )),
            Param::KeyValue {
                param: "message_attribute",
                key: prefix,
                value: module,
            } => self.message_attribute.push((
                prefix.to_string(),
                module.replace(r"\,", ",").replace(r"\\", r"\"),
            )),
            Param::Parameter {
                param: "enable_type_names",
            }
            | Param::Value {
                param: "enable_type_names",
                value: "true",
            } => self.enable_type_names = true,
            Param::Value {
                param: "enable_type_names",
                value: "false",
            } => (),
            Param::Parameter {
                param: "flat_output_dir",
            }
            | Param::Value {
                param: "flat_output_dir",
                value: "true",
            } => self.flat_output_dir = true,
            Param::Value {
                param: "flat_output_dir",
                value: "false",
            } => (),
            _ => return Err(param),
        }

        Ok(())
    }
}

/// Standard parameter regular expression
///
/// Supports the following forms:
///
/// ```text
/// parameter
/// parameter=key
/// parameter=key=value
/// ```
///
/// * `parameter` is terminated on the first `=` or `,`
/// * If `parameter` is terminated with `=`, then `key` follows, terminated by the first `=` or `,`.
/// * If `key` is terminated with `=`, then `value` follows. It is terminated only by `,`. However,
///   if that `,` is prefixed by `\` but not `\\`, then it will not terminate.
static PARAMETER: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(
        r"(?:(?P<param>[^,=]+)(?:=(?P<key>[^,=]+)(?:=(?P<value>(?:[^,\\]|\\,|\\\\)+))?)?)",
    )
    .unwrap()
});

pub struct Params<'a> {
    params: Vec<Param<'a>>,
}

impl<'a> IntoIterator for Params<'a> {
    type IntoIter = <Vec<Param<'a>> as IntoIterator>::IntoIter;
    type Item = <Vec<Param<'a>> as IntoIterator>::Item;

    fn into_iter(self) -> Self::IntoIter {
        self.params.into_iter()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Param<'a> {
    Parameter {
        param: &'a str,
    },
    Value {
        param: &'a str,
        value: &'a str,
    },
    KeyValue {
        param: &'a str,
        key: &'a str,
        value: String,
    },
}

impl<'a> Param<'a> {
    pub fn value(self) -> Option<Cow<'a, str>> {
        match self {
            Self::Parameter { .. } => None,
            Self::Value { value, .. } => Some(Cow::Borrowed(value)),
            Self::KeyValue { value, .. } => Some(Cow::Owned(value)),
        }
    }
}

impl From<Param<'_>> for InvalidParameter {
    fn from(param: Param<'_>) -> Self {
        let message = match param {
            Param::Parameter { param } => param.to_owned(),
            Param::Value { param, value } => format!("{param}={value}"),
            Param::KeyValue { param, key, value } => {
                let value = value.replace('\\', r"\\").replace(',', r"\,");
                format!("{param}={key}={value}")
            }
        };
        InvalidParameter(message)
    }
}

impl<'a> Params<'a> {
    pub fn from_protoc_plugin_opts(s: &'a str) -> std::result::Result<Self, InvalidParameter> {
        let params = PARAMETER
            .captures_iter(s)
            .map(|capture| {
                let param = capture
                    .get(1)
                    .expect("any captured group will at least have the param name")
                    .as_str()
                    .trim();

                let key = capture.get(2).map(|m| m.as_str());
                let value = capture.get(3).map(|m| m.as_str());

                match (key, value) {
                    (None, None) => Ok(Param::Parameter { param }),
                    (Some(value), None) => Ok(Param::Value { param, value }),
                    (Some(key), Some(value)) => Ok(Param::KeyValue {
                        param,
                        key,
                        value: value.replace(r"\,", ",").replace(r"\\", r"\"),
                    }),
                    _ => Err(InvalidParameter(
                        capture.get(0).unwrap().as_str().to_string(),
                    )),
                }
            })
            .collect::<std::result::Result<_, _>>()?;
        Ok(Self { params })
    }
}

impl str::FromStr for Parameters {
    type Err = InvalidParameter;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut ret_val = Self::default();
        for param in Params::from_protoc_plugin_opts(s)? {
            if let Err(param) = ret_val.prost.try_handle_parameter(param) {
                match param {
                    Param::Parameter {
                        param: "file_descriptor_set",
                    }
                    | Param::Value {
                        param: "file_descriptor_set",
                        value: "true",
                    } => ret_val.file_descriptor_set = true,
                    Param::Value {
                        param: "file_descriptor_set",
                        value: "false",
                    } => (),
                    Param::Parameter {
                        param: "generate_files_only",
                    }
                    | Param::Value {
                        param: "generate_files_only",
                        value: "true",
                    } => ret_val.generate_files_only = true,
                    Param::Value {
                        param: "generate_files_only",
                        value: "false",
                    } => (),
                    Param::Parameter {
                        param: "prost_reflect",
                    }
                    | Param::Value {
                        param: "prost_reflect",
                        value: "true",
                    } => ret_val.prost_reflect = Some(ProstReflect::Embedded),
                    Param::Value {
                        param: "prost_reflect",
                        value: "linked",
                    } => ret_val.prost_reflect = Some(ProstReflect::Linked),
                    Param::Value {
                        param: "prost_reflect",
                        value: "false",
                    } => (),
                    _ => return Err(InvalidParameter::from(param)),
                }
            }
        }

        Ok(ret_val)
    }
}

/// An invalid parameter
#[derive(Debug)]
pub struct InvalidParameter(String);

impl InvalidParameter {
    pub fn new(message: String) -> Self {
        Self(message)
    }
}

impl fmt::Display for InvalidParameter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("invalid parameter: ")?;
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidParameter {}

/// A wire-compatible reader of a [`CodeGeneratorRequest`]
///
/// This type treats the proto files contained in the request as raw byte
/// arrays so that we can round-trip those bytes into the generated files
/// as an encoded [`FileDescriptorSet`].
///
/// [`CodeGeneratorRequest`]: prost_types::compiler::CodeGeneratorRequest
/// [`FileDescriptorSet`]: prost_types::FileDescriptorSet
#[derive(Clone, PartialEq, ::prost::Message)]
struct RawProtos {
    #[prost(bytes = "vec", repeated, tag = "15")]
    proto_file: Vec<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn compiler_option_string_with_three_plus_equals_parses_correctly() {
        const INPUT: &str = r#"flat_output_dir,enable_type_names,compile_well_known_types,disable_comments=.,skip_debug=.,extern_path=.google.protobuf=::pbjson_types,type_attribute=.=#[cfg(all(feature = "test"\, feature = "orange"))]"#;

        let expected: &[Param] = &[
            Param::Parameter {
                param: "flat_output_dir",
            },
            Param::Parameter {
                param: "enable_type_names",
            },
            Param::Parameter {
                param: "compile_well_known_types",
            },
            Param::Value {
                param: "disable_comments",
                value: ".",
            },
            Param::Value {
                param: "skip_debug",
                value: ".",
            },
            Param::KeyValue {
                param: "extern_path",
                key: ".google.protobuf",
                value: "::pbjson_types".into(),
            },
            Param::KeyValue {
                param: "type_attribute",
                key: ".",
                value: r#"#[cfg(all(feature = "test", feature = "orange"))]"#.into(),
            },
        ];

        let actual = Params::from_protoc_plugin_opts(INPUT).unwrap();
        assert_eq!(actual.params, expected);
    }

    #[test]
    fn prost_reflect_applies_to_nested_messages() {
        use prost::Message;
        use prost_types::{
            compiler::CodeGeneratorRequest,
            field_descriptor_proto::{Label, Type},
            FieldDescriptorProto,
        };

        let request = CodeGeneratorRequest {
            file_to_generate: vec!["example.proto".to_owned()],
            parameter: Some("file_descriptor_set,prost_reflect".to_owned()),
            proto_file: vec![FileDescriptorProto {
                name: Some("example.proto".to_owned()),
                package: Some("example".to_owned()),
                message_type: vec![DescriptorProto {
                    name: Some("TestMessage".to_owned()),
                    nested_type: vec![DescriptorProto {
                        name: Some("MyInnerMessage".to_owned()),
                        field: vec![FieldDescriptorProto {
                            name: Some("value".to_owned()),
                            number: Some(1),
                            r#type: Some(Type::String as i32),
                            label: Some(Label::Optional as i32),
                            ..Default::default()
                        }],
                        nested_type: vec![DescriptorProto {
                            name: Some("MyDeepestMessage".to_owned()),
                            field: vec![FieldDescriptorProto {
                                name: Some("value".to_owned()),
                                number: Some(1),
                                r#type: Some(Type::String as i32),
                                label: Some(Label::Optional as i32),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let encoded = request.encode_to_vec();
        let files = execute(&encoded).expect("code generation failed");

        let example_rs = files
            .iter()
            .find(|f| f.name.as_deref() == Some("example/example.rs"))
            .map(|f| f.content.as_deref().unwrap_or_default())
            .expect("example/example.rs not generated");

        assert!(
            example_rs.contains(r#"#[prost_reflect(message_name = "example.TestMessage")]"#),
            "outer message should derive ReflectMessage:\n{}",
            example_rs
        );
        assert!(
            example_rs.contains(
                r#"#[prost_reflect(message_name = "example.TestMessage.MyInnerMessage")]"#
            ),
            "nested message should derive ReflectMessage:\n{}",
            example_rs
        );
        assert!(
            example_rs.contains(
                r#"#[prost_reflect(message_name = "example.TestMessage.MyInnerMessage.MyDeepestMessage")]"#
            ),
            "doubly-nested message should derive ReflectMessage:\n{}",
            example_rs
        );

        // The `file_descriptor_set_bytes` path must be qualified with one `super::` per level of
        // message nesting, since FILE_DESCRIPTOR_SET is declared at the package-module level while
        // nested messages live inside `pub mod <parent>` wrappers. This is the fix for
        // https://github.com/neoeinstein/protoc-gen-prost/issues/139.
        assert!(
            example_rs
                .contains(r#"#[prost_reflect(file_descriptor_set_bytes = "FILE_DESCRIPTOR_SET")]"#),
            "top-level message should reference FILE_DESCRIPTOR_SET directly:\n{}",
            example_rs
        );
        assert!(
            example_rs.contains(
                r#"#[prost_reflect(file_descriptor_set_bytes = "super::FILE_DESCRIPTOR_SET")]"#
            ),
            "singly-nested message should reference super::FILE_DESCRIPTOR_SET:\n{}",
            example_rs
        );
        assert!(
            example_rs.contains(
                r#"#[prost_reflect(file_descriptor_set_bytes = "super::super::FILE_DESCRIPTOR_SET")]"#
            ),
            "doubly-nested message should reference super::super::FILE_DESCRIPTOR_SET:\n{}",
            example_rs
        );
    }

    fn file(
        name: &str,
        package: &str,
        messages: &[&str],
        dependencies: &[&str],
    ) -> FileDescriptorProto {
        FileDescriptorProto {
            name: Some(name.to_owned()),
            package: Some(package.to_owned()),
            dependency: dependencies.iter().map(|&name| name.to_owned()).collect(),
            message_type: messages
                .iter()
                .map(|&name| DescriptorProto {
                    name: Some(name.to_owned()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// The code generated for a1.proto and a2.proto (package `a`), b.proto (package `b`) and their
    /// imports with `parameter`, by output file name.
    fn generate_linked(parameter: &str) -> HashMap<String, String> {
        use prost_types::compiler::CodeGeneratorRequest;

        let mut a1 = file(
            "a1.proto",
            "a",
            &["A1"],
            &["ext.proto", "opts.proto", "google/protobuf/timestamp.proto"],
        );
        a1.message_type[0].nested_type.push(DescriptorProto {
            name: Some("Inner".to_owned()),
            ..Default::default()
        });
        let request = CodeGeneratorRequest {
            file_to_generate: vec![
                "a1.proto".to_owned(),
                "b.proto".to_owned(),
                "a2.proto".to_owned(),
            ],
            parameter: Some(parameter.to_owned()),
            proto_file: vec![
                file(
                    "google/protobuf/timestamp.proto",
                    "google.protobuf",
                    &["Timestamp"],
                    &[],
                ),
                file("ext.proto", "ext", &["E"], &[]),
                // Only extensions, which no extern_path maps.
                file("opts.proto", "opts", &[], &[]),
                a1,
                // Packages `a` and `b` import each other.
                file("b.proto", "b", &["B"], &["a1.proto"]),
                file("a2.proto", "a", &["A2"], &["b.proto"]),
            ],
            ..Default::default()
        };
        let mut contents = HashMap::<String, String>::new();
        for file in execute(&request.encode_to_vec()).expect("code generation failed") {
            contents
                .entry(file.name.unwrap_or_default())
                .or_default()
                .push_str(&file.content.unwrap_or_default());
        }
        contents
    }

    #[test]
    fn prost_reflect_linked_registers_generated_and_imported_files() {
        let contents = generate_linked(
            "file_descriptor_set,prost_reflect=linked,extern_path=.ext=::ext_crate::ext",
        );
        let a = &contents["a/a.rs"];
        let b = &contents["b/b.rs"];

        assert!(
            a.contains(r#"#[prost_reflect(descriptor_pool = "__descriptor_pool()")]"#),
            "top-level messages should read their module's pool:\n{a}"
        );
        assert!(
            a.contains(r#"#[prost_reflect(descriptor_pool = "super::__descriptor_pool()")]"#),
            "nested messages should read their package's module's pool:\n{a}"
        );
        for module in [a, b] {
            assert!(
                module.contains("::ext_crate::ext::register_descriptors();"),
                "the module the extern_path maps should register first:\n{module}"
            );
            assert!(
                !module.contains("google"),
                "the well-known types are in the pool from the start:\n{module}"
            );
        }
        assert!(
            a.contains(
                "[FILE_DESCRIPTOR_SET, super::b::FILE_DESCRIPTOR_SET, \
                 IMPORTED_FILE_DESCRIPTOR_SET]"
            ),
            "every generated module's files should be added together:\n{a}"
        );
        assert!(
            b.contains(
                "[super::a::FILE_DESCRIPTOR_SET, FILE_DESCRIPTOR_SET, \
                 super::a::IMPORTED_FILE_DESCRIPTOR_SET]"
            ),
            "every generated module's files should be added together:\n{b}"
        );
        assert!(
            a.contains("const IMPORTED_FILE_DESCRIPTOR_SET")
                && !b.contains("const IMPORTED_FILE_DESCRIPTOR_SET"),
            "the unmapped imports should be embedded once"
        );
        let bytes: Vec<u8> = a
            .split("const IMPORTED_FILE_DESCRIPTOR_SET: &[u8] = &[")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("IMPORTED_FILE_DESCRIPTOR_SET bytes")
            .split(',')
            .map(|byte| byte.trim().parse().unwrap())
            .collect();
        let imported = prost_types::FileDescriptorSet::decode(bytes.as_slice()).unwrap();
        assert_eq!(
            imported
                .file
                .iter()
                .map(|file| file.name())
                .collect::<Vec<_>>(),
            ["opts.proto"],
            "only the unmapped import should be embedded, as a FileDescriptorSet"
        );
    }

    /// As rules_rust passes them: an extern_path per type.
    #[test]
    fn prost_reflect_linked_resolves_extern_paths_of_types() {
        let contents = generate_linked(
            "file_descriptor_set,prost_reflect=linked,extern_path=.ext.E=::ext_crate::ext::E",
        );
        assert!(contents["a/a.rs"].contains("::ext_crate::ext::register_descriptors();"));
    }

    /// Package `split` spread over three files, as when each is in a crate of its own: c.proto,
    /// generated, imports b.proto, mapped by an extern_path per type as rules_rust passes them,
    /// which imports a.proto, which nothing maps.
    fn generate_split(parameter: &str) -> HashMap<String, String> {
        use prost_types::{
            compiler::CodeGeneratorRequest,
            field_descriptor_proto::{Label, Type},
            FieldDescriptorProto,
        };

        let field = |name: &str, r#type: Type, type_name: Option<&str>| FieldDescriptorProto {
            name: Some(name.to_owned()),
            number: Some(1),
            r#type: Some(r#type as i32),
            type_name: type_name.map(str::to_owned),
            label: Some(Label::Optional as i32),
            ..Default::default()
        };
        let mut a = file("a.proto", "split", &["A"], &[]);
        a.message_type[0].field = vec![field("value", Type::Float, None)];
        let mut b = file("b.proto", "split", &["B"], &["a.proto"]);
        b.message_type[0].field = vec![field("a", Type::Message, Some(".split.A"))];
        let mut c = file("c.proto", "split", &["C"], &["b.proto"]);
        c.message_type[0].field = vec![field("b", Type::Message, Some(".split.B"))];

        let request = CodeGeneratorRequest {
            file_to_generate: vec!["c.proto".to_owned()],
            parameter: Some(format!(
                "{parameter},extern_path=.split.B=::b_crate::split::B"
            )),
            proto_file: vec![a, b, c],
            ..Default::default()
        };
        execute(&request.encode_to_vec())
            .expect("code generation failed")
            .into_iter()
            .map(|file| {
                (
                    file.name.unwrap_or_default(),
                    file.content.unwrap_or_default(),
                )
            })
            .fold(HashMap::new(), |mut contents, (name, content)| {
                contents
                    .entry(name)
                    .or_insert_with(String::new)
                    .push_str(&content);
                contents
            })
    }

    #[test]
    fn generate_files_only_skips_the_other_files_of_the_package() {
        let split = &generate_split("generate_files_only,file_descriptor_set")["split/split.rs"];

        assert!(split.contains("pub struct C "), "{split}");
        assert!(
            !split.contains("pub struct A ") && !split.contains("pub struct B "),
            "the files not to generate should not be:\n{split}"
        );
        assert!(
            split.contains("::b_crate::split::B"),
            "the caller's extern_path should be kept:\n{split}"
        );
        assert!(
            !split.contains("not_generated"),
            "nothing should refer to the types of the files not to generate:\n{split}"
        );
        // The derives of the struct C, i.e. the last derive line before it.
        let derives = |module: &str| {
            module
                .split("pub struct C ")
                .next()
                .and_then(|before| before.lines().rev().find(|line| line.contains("#[derive(")))
                .unwrap_or_default()
                .to_owned()
        };
        assert_eq!(
            derives(split),
            derives(&generate_split("file_descriptor_set")["split/split.rs"]),
            "prost should derive the same traits on C as when it generates the whole package"
        );

        let bytes: Vec<u8> = split
            .split("pub const FILE_DESCRIPTOR_SET: &[u8] = &[")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("FILE_DESCRIPTOR_SET bytes")
            .split(',')
            .map(str::trim)
            .filter(|byte| !byte.is_empty())
            .map(|byte| u8::from_str_radix(byte.trim_start_matches("0x"), 16).unwrap())
            .collect();
        let set = prost_types::FileDescriptorSet::decode(bytes.as_slice()).unwrap();
        assert_eq!(
            set.file.iter().map(|file| file.name()).collect::<Vec<_>>(),
            ["c.proto"],
            "FILE_DESCRIPTOR_SET should hold the generated files only"
        );
    }

    #[test]
    fn without_generate_files_only_the_whole_package_is_generated() {
        let split = &generate_split("file_descriptor_set")["split/split.rs"];

        assert!(
            split.contains("pub struct A ") && split.contains("pub struct C "),
            "{split}"
        );
    }
}
