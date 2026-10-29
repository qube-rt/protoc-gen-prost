use std::collections::{BTreeSet, HashMap, HashSet};

use prost::Message;
use prost_build::Module;
use prost_types::{compiler::CodeGeneratorRequest, FileDescriptorProto};

use super::file_descriptor_set::RawProtosSet;
use crate::{Generator, ModuleRequestSet, RawProtos, Result};

/// Appends to every generated module the registration of the descriptors of the generated files in
/// prost-reflect's global pool, for `prost_reflect=linked`.
///
/// Each module embeds the descriptors of its own files only (`FILE_DESCRIPTOR_SET`). Its
/// `register_descriptors()` first calls the `register_descriptors()` of the modules the
/// `extern_path`s map the imported files to, then adds the descriptors of every generated module:
/// the generated packages can import each other, so they are added together. The `ReflectMessage`
/// implementations read the pool through `__descriptor_pool()`, which registers first.
pub struct RegistrationGenerator {
    /// The modules, outside of the generated ones, whose `register_descriptors()` adds the files
    /// the generated ones import.
    imported_modules: BTreeSet<String>,
    /// The imported files that no `extern_path` maps, as they define no message or enum, only
    /// extensions: an encoded `FileDescriptorSet` of them, registered with the generated files.
    imported_files: Option<Vec<u8>>,
}

impl RegistrationGenerator {
    /// Resolves the files `request` imports to the modules that register them
    pub fn new(
        request: &CodeGeneratorRequest,
        raw_protos: &RawProtos,
        extern_paths: &[(String, String)],
    ) -> Self {
        let files: HashMap<&str, (&FileDescriptorProto, &[u8])> = request
            .proto_file
            .iter()
            .zip(&raw_protos.proto_file)
            .map(|(file, raw)| (file.name(), (file, raw.as_slice())))
            .collect();
        let mut imports = Imports {
            files: &files,
            generated: request
                .file_to_generate
                .iter()
                .map(String::as_str)
                .collect(),
            extern_paths,
            modules: BTreeSet::new(),
            unmapped: Vec::new(),
        };
        for name in &request.file_to_generate {
            imports.add_dependencies(files[name.as_str()].0);
        }

        let imported_files = (!imports.unmapped.is_empty()).then(|| {
            RawProtosSet {
                file: imports
                    .unmapped
                    .iter()
                    .map(|name| files[name.as_str()].1.to_owned())
                    .collect(),
            }
            .encode_to_vec()
        });
        Self {
            imported_modules: imports.modules,
            imported_files,
        }
    }
}

impl Generator for RegistrationGenerator {
    fn generate(&mut self, module_request_set: &ModuleRequestSet) -> Result {
        let generated: Vec<&Module> = module_request_set
            .requests()
            .filter(|(_, request)| request.output_filename().is_some())
            .map(|(module, _)| module)
            .collect();
        let home = match generated.first() {
            Some(&home) => home,
            None => return Ok(Vec::new()),
        };

        let files = module_request_set
            .requests()
            .filter_map(|(module, request)| {
                request.append_to_file(|buffer| {
                    if module == home {
                        if let Some(imported_files) = &self.imported_files {
                            buffer.push_str(&format!(
                                "/// The imported files that no `extern_path` maps.\nconst \
                                 IMPORTED_FILE_DESCRIPTOR_SET: &[u8] = &{imported_files:?};\n"
                            ));
                        }
                    }
                    let mut descriptor_sets: Vec<String> = generated
                        .iter()
                        .map(|other| item_path(module, other, "FILE_DESCRIPTOR_SET"))
                        .collect();
                    if self.imported_files.is_some() {
                        descriptor_sets.push(item_path(
                            module,
                            home,
                            "IMPORTED_FILE_DESCRIPTOR_SET",
                        ));
                    }
                    buffer.push_str(&registration(&self.imported_modules, &descriptor_sets));
                })
            })
            .collect();
        Ok(files)
    }
}

/// The imports of the generated files
struct Imports<'a> {
    files: &'a HashMap<&'a str, (&'a FileDescriptorProto, &'a [u8])>,
    generated: HashSet<&'a str>,
    extern_paths: &'a [(String, String)],
    modules: BTreeSet<String>,
    /// In the order they can be added in: each after the ones it imports
    unmapped: Vec<String>,
}

impl Imports<'_> {
    fn add_dependencies(&mut self, file: &FileDescriptorProto) {
        for dependency in &file.dependency {
            let (dependency, _) = self.files[dependency.as_str()];
            // prost-reflect's global pool starts with the well-known types.
            if dependency.package() == "google.protobuf"
                || self.generated.contains(dependency.name())
                || self.unmapped.iter().any(|name| name == dependency.name())
            {
                continue;
            }
            if let Some(module) = extern_module(dependency, self.extern_paths) {
                self.modules.insert(module);
            } else {
                self.add_dependencies(dependency);
                self.unmapped.push(dependency.name().to_owned());
            }
        }
    }
}

/// The module the `extern_path`s map the package of `file` to: through the package or one of its
/// ancestors, or through one of its top-level types.
fn extern_module(file: &FileDescriptorProto, extern_paths: &[(String, String)]) -> Option<String> {
    let package = format!(".{}", file.package());
    let mapped_package = extern_paths
        .iter()
        .filter(|(proto_path, _)| {
            package == *proto_path || package.starts_with(&format!("{proto_path}."))
        })
        .max_by_key(|(proto_path, _)| proto_path.len());
    if let Some((proto_path, rust_path)) = mapped_package {
        let rest = Module::from_protobuf_package_name(&package[proto_path.len()..]);
        return Some(
            std::iter::once(rust_path.as_str())
                .chain(rest.parts())
                .collect::<Vec<_>>()
                .join("::"),
        );
    }
    let type_names = file
        .message_type
        .iter()
        .map(|message| message.name())
        .chain(file.enum_type.iter().map(|enumeration| enumeration.name()));
    for type_name in type_names {
        let type_path = format!("{package}.{type_name}");
        if let Some((_, rust_path)) = extern_paths
            .iter()
            .find(|(proto_path, _)| *proto_path == type_path)
        {
            return rust_path
                .rsplit_once("::")
                .map(|(module, _)| module.to_owned());
        }
    }
    None
}

/// The path to `item` of module `to` from module `from`, through their common ancestor as prost
/// does for the types of other packages.
fn item_path(from: &Module, to: &Module, item: &str) -> String {
    let from: Vec<&str> = from.parts().collect();
    let to: Vec<&str> = to.parts().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    std::iter::repeat("super")
        .take(from.len() - common)
        .chain(to[common..].iter().copied())
        .chain(std::iter::once(item))
        .collect::<Vec<_>>()
        .join("::")
}

fn registration(imported_modules: &BTreeSet<String>, descriptor_sets: &[String]) -> String {
    let register_imports: String = imported_modules
        .iter()
        .map(|module| format!("        {module}::register_descriptors();\n"))
        .collect();
    let descriptor_sets = descriptor_sets.join(", ");
    format!(
        r"
/// Adds the descriptors of the generated files to prost-reflect's global pool, once, after those
/// of the files they import. Skips the files the pool already has.
pub fn register_descriptors() {{
    static REGISTER: ::std::sync::Once = ::std::sync::Once::new();
    REGISTER.call_once(|| {{
{register_imports}        // Encoded FileDescriptorSets concatenate into one holding all their files. Decoding
        // them in the pool, rather than with prost-types first, keeps the extensions set in their
        // options.
        ::prost_reflect::DescriptorPool::decode_global_file_descriptor_set(
            [{descriptor_sets}].concat().as_slice(),
        )
        .unwrap();
    }});
}}

/// The pool the `ReflectMessage` implementations of this module's messages read.
#[doc(hidden)]
pub fn __descriptor_pool() -> ::prost_reflect::DescriptorPool {{
    register_descriptors();
    ::prost_reflect::DescriptorPool::global()
}}
"
    )
}
