use nu_engine::{current_dir, eval_block, CallExt};
use nu_protocol::ast::Call;
use nu_protocol::engine::{Command, EngineState, Stack};
use nu_protocol::util::BufferedReader;
use nu_protocol::{
    Category, Example, IntoInterruptiblePipelineData, PipelineData, RawStream, ShellError,
    Signature, Spanned, SyntaxShape, Type, Value,
};
use std::io::BufReader;

#[cfg(feature = "sqlite")]
use crate::database::SQLiteDatabase;

#[cfg(feature = "sqlite")]
use nu_protocol::IntoPipelineData;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[derive(Clone)]
pub struct Open;

impl Command for Open {
    fn name(&self) -> &str {
        "open"
    }

    fn usage(&self) -> &str {
        "Load a file into a cell, converting to table if possible (avoid by appending '--raw')."
    }

    fn extra_usage(&self) -> &str {
        "Support to automatically parse files with an extension `.xyz` can be provided by a `from xyz` command in scope."
    }

    fn search_terms(&self) -> Vec<&str> {
        vec!["load", "read", "load_file", "read_file"]
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("open")
            .input_output_types(vec![(Type::Nothing, Type::Any), (Type::String, Type::Any)])
            .optional("filename", SyntaxShape::Filepath, "the filename to use")
            .rest(
                "filenames",
                SyntaxShape::Filepath,
                "optional additional files to open",
            )
            .switch("raw", "open file as raw binary", Some('r'))
            .category(Category::FileSystem)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let raw = call.has_flag("raw");
        let call_span = call.head;
        let ctrlc = engine_state.ctrlc.clone();
        let cwd = current_dir(engine_state, stack)?;
        let req_path = call.opt::<Spanned<String>>(engine_state, stack, 0)?;
        let mut path_params = call.rest::<Spanned<String>>(engine_state, stack, 1)?;

        // FIXME: JT: what is this doing here?

        if let Some(filename) = req_path {
            path_params.insert(0, filename);
        } else {
            let filename = match input {
                PipelineData::Value(Value::Nothing { .. }, ..) => {
                    return Err(ShellError::MissingParameter {
                        param_name: "needs filename".to_string(),
                        span: call.head,
                    })
                }
                PipelineData::Value(val, ..) => val.as_spanned_string()?,
                _ => {
                    return Err(ShellError::MissingParameter {
                        param_name: "needs filename".to_string(),
                        span: call.head,
                    });
                }
            };

            path_params.insert(0, filename);
        }

        let mut output = vec![];

        for path in path_params.into_iter() {
            //FIXME: `open` should not have to do this
            let path = {
                Spanned {
                    item: nu_utils::strip_ansi_string_unlikely(path.item),
                    span: path.span,
                }
            };

            let arg_span = path.span;
            // let path_no_whitespace = &path.item.trim_end_matches(|x| matches!(x, '\x09'..='\x0d'));

            for path in nu_engine::glob_from(&path, &cwd, call_span, None)
                .map_err(|err| match err {
                    ShellError::DirectoryNotFound { span, .. } => ShellError::FileNotFound { span },
                    _ => err,
                })?
                .1
            {
                let path = path?;
                let path = Path::new(&path);

                if permission_denied(path) {
                    #[cfg(unix)]
                    let error_msg = match path.metadata() {
                        Ok(md) => format!(
                            "The permissions of {:o} does not allow access for this user",
                            md.permissions().mode() & 0o0777
                        ),
                        Err(e) => e.to_string(),
                    };

                    #[cfg(not(unix))]
                    let error_msg = String::from("Permission denied");
                    return Err(ShellError::GenericError(
                        "Permission denied".into(),
                        error_msg,
                        Some(arg_span),
                        None,
                        Vec::new(),
                    ));
                } else {
                    #[cfg(feature = "sqlite")]
                    if !raw {
                        let res = SQLiteDatabase::try_from_path(path, arg_span, ctrlc.clone())
                            .map(|db| db.into_value(call.head).into_pipeline_data());

                        if res.is_ok() {
                            return res;
                        }
                    }

                    let file = match std::fs::File::open(path) {
                        Ok(file) => file,
                        Err(err) => {
                            return Err(ShellError::GenericError(
                                "Permission denied".into(),
                                err.to_string(),
                                Some(arg_span),
                                None,
                                Vec::new(),
                            ));
                        }
                    };

                    let buf_reader = BufReader::new(file);

                    let file_contents = PipelineData::ExternalStream {
                        stdout: Some(RawStream::new(
                            Box::new(BufferedReader { input: buf_reader }),
                            ctrlc.clone(),
                            call_span,
                            None,
                        )),
                        stderr: None,
                        exit_code: None,
                        span: call_span,
                        metadata: None,
                        trim_end_newline: false,
                    };
                    let exts_opt: Option<Vec<String>> = if raw {
                        None
                    } else {
                        let path_str = path
                            .file_name()
                            .unwrap_or(std::ffi::OsStr::new(path))
                            .to_string_lossy()
                            .to_lowercase();
                        Some(extract_extensions(path_str.as_str()))
                    };

                    let converter = exts_opt.and_then(|exts| {
                        exts.iter().find_map(|ext| {
                            engine_state
                                .find_decl(format!("from {}", ext).as_bytes(), &[])
                                .map(|id| (id, ext.to_string()))
                        })
                    });

                    match converter {
                        Some((converter_id, ext)) => {
                            let decl = engine_state.get_decl(converter_id);
                            let command_output = if let Some(block_id) = decl.get_block_id() {
                                let block = engine_state.get_block(block_id);
                                eval_block(engine_state, stack, block, file_contents, false, false)
                            } else {
                                decl.run(engine_state, stack, &Call::new(call_span), file_contents)
                            };
                            output.push(command_output.map_err(|inner| {
                                    ShellError::GenericError(
                                        format!("Error while parsing as {ext}"),
                                        format!("Could not parse '{}' with `from {}`", path.display(), ext),
                                        Some(arg_span),
                                        Some(format!("Check out `help from {}` or `help from` for more options or open raw data with `open --raw '{}'`", ext, path.display())),
                                        vec![inner],
                                    )
                                })?);
                        }
                        None => output.push(file_contents),
                    }
                }
            }
        }

        if output.is_empty() {
            Ok(PipelineData::Empty)
        } else if output.len() == 1 {
            Ok(output.remove(0))
        } else {
            Ok(output.into_iter().flatten().into_pipeline_data(ctrlc))
        }
    }

    fn examples(&self) -> Vec<nu_protocol::Example> {
        vec![
            Example {
                description: "Open a file, with structure (based on file extension or SQLite database header)",
                example: "open myfile.json",
                result: None,
            },
            Example {
                description: "Open a file, as raw bytes",
                example: "open myfile.json --raw",
                result: None,
            },
            Example {
                description: "Open a file, using the input to get filename",
                example: "'myfile.txt' | open",
                result: None,
            },
            Example {
                description: "Open a file, and decode it by the specified encoding",
                example: "open myfile.txt --raw | decode utf-8",
                result: None,
            },
            Example {
                description: "Create a custom `from` parser to open newline-delimited JSON files with `open`",
                example: r#"def "from ndjson" [] { from json -o }; open myfile.ndjson"#,
                result: None,
            },
        ]
    }
}

fn permission_denied(dir: impl AsRef<Path>) -> bool {
    match dir.as_ref().read_dir() {
        Err(e) => matches!(e.kind(), std::io::ErrorKind::PermissionDenied),
        Ok(_) => false,
    }
}

fn extract_extensions(filename: &str) -> Vec<String> {
    let parts: Vec<&str> = filename.split('.').collect();
    let mut extensions: Vec<String> = Vec::new();
    let mut current_extension = String::new();

    for part in parts.iter().rev() {
        if current_extension.is_empty() {
            current_extension.push_str(part);
        } else {
            current_extension = format!("{}.{}", part, current_extension);
        }
        extensions.push(current_extension.clone());
    }

    extensions.pop();
    extensions.reverse();

    extensions
}
