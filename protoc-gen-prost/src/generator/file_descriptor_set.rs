use std::fmt::Write;

use prost_types::compiler::code_generator_response::File;

use crate::{Generator, ModuleRequest, ModuleRequestSet, Result};

pub struct FileDescriptorSetGenerator {
    /// Whether to include all dependent proto files in the FileDescriptorSet.
    /// When true, the generated FileDescriptorSet will include all imported proto files
    /// from all modules, which is required for prost-reflect to work correctly.
    include_all_dependencies: bool,
    /// Whether to include only the module's files that are in `file_to_generate`, rather than all
    /// its files in the request
    generated_files_only: bool,
}

impl FileDescriptorSetGenerator {
    /// Creates a new FileDescriptorSetGenerator that includes only the proto files
    /// in each module (not their dependencies), or only those that are generated
    pub fn new(generated_files_only: bool) -> Self {
        Self {
            include_all_dependencies: false,
            generated_files_only,
        }
    }

    /// Creates a new FileDescriptorSetGenerator that includes all dependent proto files
    /// from all modules. This is required when using prost-reflect.
    pub fn with_all_dependencies() -> Self {
        Self {
            include_all_dependencies: true,
            generated_files_only: false,
        }
    }
}

impl Generator for FileDescriptorSetGenerator {
    fn generate(&mut self, module_request_set: &ModuleRequestSet) -> Result {
        let files = module_request_set
            .requests()
            .filter_map(|(_, request)| {
                if self.include_all_dependencies {
                    Self::generate_all_dependent(request, module_request_set)
                } else {
                    Self::generate_one(request, self.generated_files_only)
                }
            })
            .collect();

        Ok(files)
    }
}

impl FileDescriptorSetGenerator {
    /// Generates a FileDescriptorSet containing only the proto files in the current module
    fn generate_one(request: &ModuleRequest, generated_files_only: bool) -> Option<File> {
        request.append_to_file(|buffer| {
            // This cannot be done with another file and `include_bytes!` because the
            // contract for a file's contents requires that they be valid UTF-8.
            //
            // So, we append them as an embedded array instead.
            append_file_descriptor_set_bytes(
                request.proto_package_name(),
                &RawProtosSet {
                    file: request
                        .raw_files()
                        .zip(request.files_to_generate())
                        .filter(|(_, to_generate)| *to_generate || !generated_files_only)
                        .map(|(raw, _)| raw.to_owned())
                        .collect(),
                },
                buffer,
            );
        })
    }

    /// Generates a FileDescriptorSet containing all proto files from all modules.
    /// This ensures imported dependencies are included, which is required for prost-reflect.
    fn generate_all_dependent(
        request: &ModuleRequest,
        module_request_set: &ModuleRequestSet,
    ) -> Option<File> {
        request.append_to_file(|buffer| {
            // This cannot be done with another file and `include_bytes!` because the
            // contract for a file's contents requires that they be valid UTF-8.
            //
            // So, we append them as an embedded array instead.
            //
            // Collect all raw proto files from all modules to ensure imported dependencies
            // are included in the FileDescriptorSet
            let all_raw_files: Vec<Vec<u8>> = module_request_set
                .requests()
                .flat_map(|(_, req)| req.raw_files().map(|b| b.to_owned()))
                .collect();

            append_file_descriptor_set_bytes(
                request.proto_package_name(),
                &RawProtosSet {
                    file: all_raw_files,
                },
                buffer,
            );
        })
    }
}

/// Wire-compatible FileDescriptorSet that doesn't require fully-decoded file descriptors
#[derive(Clone, PartialEq, ::prost::Message)]
pub(crate) struct RawProtosSet {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub(crate) file: Vec<Vec<u8>>,
}

fn append_file_descriptor_set_bytes(
    package: &str,
    file_descriptor_set: &impl prost::Message,
    buffer: &mut String,
) {
    buffer.push_str("/// Encoded file descriptor set for the `");
    buffer.push_str(package);
    buffer.push_str("` package\n");

    buffer.push_str("pub const FILE_DESCRIPTOR_SET: &[u8] = &[\n");

    let encoded = file_descriptor_set.encode_to_vec();
    let mut chunks = encoded.chunks_exact(16);
    for chunck in chunks.by_ref() {
        writeln!(
            buffer,
            "    {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, \
             {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x}, {:#04x},",
            chunck[0],
            chunck[1],
            chunck[2],
            chunck[3],
            chunck[4],
            chunck[5],
            chunck[6],
            chunck[7],
            chunck[8],
            chunck[9],
            chunck[10],
            chunck[11],
            chunck[12],
            chunck[13],
            chunck[14],
            chunck[15],
        )
        .unwrap();
    }

    if !chunks.remainder().is_empty() {
        buffer.push_str("    ");
        for byte in chunks.remainder() {
            write!(buffer, "{byte:#04x}, ").unwrap();
        }
        let _ = buffer.pop();
        buffer.push('\n');
    }

    buffer.push_str("];\n");
}
