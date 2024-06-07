use std::{
    collections::HashMap,
    fs::Permissions,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use axum::{http::StatusCode, response::IntoResponse, Json};
use crate::{
    limits::SystemLimits,
    requests::AddRuntimeRequest,
};
use rusqlite::Connection;
use tokio::{
    fs,
    process::Command,
    sync::{RwLock, Semaphore},
    task,
};


const MAX_BOX_ID: u64 = 900;
const METADATA_FILE_NAME: &str = "metadata.txt";
const DB_PATH: &str = "/envicutor/runtimes/db.sql";

#[derive(serde::Serialize)]
struct Message {
    message: String,
}

#[derive(serde::Serialize)]
struct StageResult {
    memory: Option<u32>,
    exit_code: Option<u32>,
    exit_signal: Option<u32>,
    exit_message: Option<String>,
    exit_status: Option<String>,
    stdout: String,
    stderr: String,
    cpu_time: Option<u32>,
    wall_time: Option<u32>,
}

fn split_metadata_line(line: &str) -> (Result<&str, ()>, Result<&str, ()>) {
    let mut entry: Vec<&str> = line.split(':').collect();
    let value = match entry.pop() {
        Some(e) => Ok(e),
        None => Err(()),
    };
    let key = match entry.pop() {
        Some(e) => Ok(e),
        None => Err(()),
    };

    (key, value)
}

pub async fn install_package(
    system_limits: SystemLimits,
    semaphore: Arc<Semaphore>,
    box_id: Arc<AtomicU64>,
    metadata_cache: Arc<RwLock<HashMap<u32, String>>>,
    Json(req): Json<AddRuntimeRequest>,
) -> Result<impl IntoResponse, (StatusCode, impl IntoResponse)> {
    let bad_request_message = if req.name.is_empty() {
        "Name can't be empty"
    } else if req.nix_shell.is_empty() {
        "Nix shell can't be empty"
    } else if req.run_script.is_empty() {
        "Run command can't be empty"
    } else if req.source_file_name.is_empty() {
        "Source file name can't be empty"
    } else {
        ""
    };
    if !bad_request_message.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(Message {
                message: bad_request_message.to_string(),
            })
            .into_response(),
        ));
    }

    let current_box_id = box_id.fetch_add(1, Ordering::SeqCst) % MAX_BOX_ID;
    Command::new("isolate")
        .args(["--init", "--cg", &format!("-b{}", current_box_id)])
        .output()
        .await
        .map_err(|e| {
            eprintln!("Could not create isolate environment: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;

    let workdir = format!("/tmp/{current_box_id}");
    if let Ok(true) = fs::try_exists(&workdir).await {
        fs::remove_dir_all(&workdir).await.map_err(|e| {
            eprintln!("Failed to remove: {workdir}, error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;
    }
    fs::create_dir(&workdir).await.map_err(|e| {
        eprintln!("Failed to create: {workdir}, error: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(Message {
                message: "Internal server error".to_string(),
            }),
        )
    })?;

    let nix_shell_path = format!("{workdir}/box/shell.nix");
    fs::write(&nix_shell_path, &req.nix_shell)
        .await
        .map_err(|e| {
            eprintln!("Could not create nix shell: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;

    let metadata_file_path = format!("{workdir}/{METADATA_FILE_NAME}");
    let limits = req
        .get_limits(&system_limits.installation)
        .map_err(|message| (StatusCode::BAD_REQUEST, Json(Message { message })))?;

    let permit = semaphore.acquire().await.map_err(|e| {
        eprintln!("Failed to acquire semaphore: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(Message {
                message: "Internal server error".to_string(),
            }),
        )
    })?;

    let cmd_res = Command::new("isolate")
        .args([
            "--run",
            &format!("--meta={}", metadata_file_path),
            "--cg",
            "--dir=/nix/store:rw,dev",
            &format!("--dir={}", workdir),
            &format!("--cg-mem={}", limits.memory),
            &format!("--wall-time={}", limits.wall_time),
            &format!("--time={}", limits.cpu_time),
            &format!("--extra-time={}", limits.extra_time),
            &format!("--open-files={}", limits.max_open_files),
            &format!("--fsize={}", limits.max_file_size),
            &format!("--processes={}", limits.max_number_of_processes),
            &format!("-b{}", current_box_id),
            "--",
            "nix-shell",
            &format!("{}/shell.nix", workdir),
            "--run",
            "export",
        ])
        .output()
        .await
        .map_err(|e| {
            eprintln!("Failed to run isolate command: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;

    if cmd_res.status.success() {
        let runtime_name = req.name.clone();
        let source_file_name = req.source_file_name;

        let runtime_id: u32 = task::spawn_blocking(move || {
            let connection = Connection::open(DB_PATH).map_err(|e| {
                eprintln!("Failed to open SQLite connection: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;

            connection
                .execute(
                    "INSERT INTO runtime (name, source_file_name) VALUES (?, ?)",
                    (runtime_name, source_file_name),
                )
                .map_err(|e| {
                    eprintln!("Failed to execute statement: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?;

            let row_id = connection
                .query_row("SELECT last_insert_rowid()", (), |row| row.get(0))
                .map_err(|e| {
                    eprintln!("Failed to get last inserted row id: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?;

            Ok::<u32, (StatusCode, Json<Message>)>(row_id)
        })
        .await
        .map_err(|e| {
            eprintln!("Failed to spawn blocking task: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })??;

        let runtime_dir = format!("/envicutor/runtimes/{runtime_id}");
        // Consider abstracting if more duplications of exists+remove+create arise
        if let Ok(true) = fs::try_exists(&runtime_dir).await {
            fs::remove_dir_all(&runtime_dir).await.map_err(|e| {
                eprintln!("Failed to remove: {runtime_dir}, error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;
        }
        fs::create_dir(&runtime_dir).await.map_err(|e| {
            eprintln!("Failed to create: {runtime_dir}, error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;

        if !req.compile_script.is_empty() {
            let compile_script_path = format!("{runtime_dir}/compile");
            fs::write(&compile_script_path, req.compile_script)
                .await
                .map_err(|e| {
                    eprintln!("Failed to write compile script: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?;
            fs::set_permissions(&compile_script_path, Permissions::from_mode(0o755))
                .await
                .map_err(|e| {
                    eprintln!("Failed to set permissions on compile script: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?;
        }

        let run_script_path = format!("{runtime_dir}/run");
        fs::write(&run_script_path, &req.run_script)
            .await
            .map_err(|e| {
                eprintln!("Failed to write run script: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;
        fs::set_permissions(&run_script_path, Permissions::from_mode(0o755))
            .await
            .map_err(|e| {
                eprintln!("Failed to set permissions on run script: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;

        let env_script_path = format!("{runtime_dir}/env");
        fs::write(&env_script_path, &cmd_res.stdout)
            .await
            .map_err(|e| {
                eprintln!("Failed to write env script: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;
        fs::set_permissions(&env_script_path, Permissions::from_mode(0o755))
            .await
            .map_err(|e| {
                eprintln!("Failed to set permissions on env script: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;

        fs::write(&(format!("{runtime_dir}/shell.nix")), &req.nix_shell)
            .await
            .map_err(|e| {
                eprintln!("Failed to write shell.nix: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Message {
                        message: "Internal server error".to_string(),
                    }),
                )
            })?;

        let mut metadata_guard = metadata_cache.write().await;
        metadata_guard.insert(runtime_id, req.name);
        drop(metadata_guard);
    }

    drop(permit);

    let mut memory: Option<u32> = None;
    let mut exit_code: Option<u32> = None;
    let mut exit_signal: Option<u32> = None;
    let mut exit_message: Option<String> = None;
    let mut exit_status: Option<String> = None;
    let mut cpu_time: Option<u32> = None;
    let mut wall_time: Option<u32> = None;
    let metadata_str = fs::read_to_string(metadata_file_path).await.map_err(|e| {
        eprintln!("Error reading metadata file: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(Message {
                message: "Internal server error".to_string(),
            }),
        )
    })?;
    let metadata_lines = metadata_str.lines();
    for line in metadata_lines {
        let (key_res, value_res) = split_metadata_line(line);
        let key = key_res.map_err(|_| {
            eprintln!("Failed to parse metadata file, received: {line}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;
        let value = value_res.map_err(|_| {
            eprintln!("Failed to parse metadata file, received: {line}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Message {
                    message: "Internal server error".to_string(),
                }),
            )
        })?;
        match key {
            "cgmem" => {
                memory = Some(value.parse().map_err(|_| {
                    eprintln!("Failed to parse memory usage, received value: {value}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?)
            }
            "exitcode" => {
                exit_code = Some(value.parse().map_err(|_| {
                    eprintln!("Failed to parse exit code, received value: {value}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?)
            }
            "exitsig" => {
                exit_signal = Some(value.parse().map_err(|_| {
                    eprintln!("Failed to parse exit signal, received value: {value}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?)
            }
            "message" => exit_message = Some(value.to_string()),
            "status" => exit_status = Some(value.to_string()),
            "time" => {
                cpu_time = Some(value.parse().map_err(|_| {
                    eprintln!("Failed to parse cpu time, received value: {value}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?)
            }
            "time-wall" => {
                wall_time = Some(value.parse().map_err(|_| {
                    eprintln!("Failed to parse wall time, received value: {value}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(Message {
                            message: "Internal server error".to_string(),
                        }),
                    )
                })?)
            }
            _ => {}
        }
    }
    let result = StageResult {
        cpu_time,
        exit_code,
        exit_message,
        exit_signal,
        exit_status,
        memory,
        stderr: String::from_utf8_lossy(&cmd_res.stderr).to_string(),
        stdout: String::from_utf8_lossy(&cmd_res.stdout).to_string(),
        wall_time,
    };

    Ok((StatusCode::OK, Json(result).into_response()))
}
