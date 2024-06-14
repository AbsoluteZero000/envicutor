use anyhow::{anyhow, Error};
use tokio::{fs, process::Command};

use crate::limits::MandatoryLimits;

pub struct Isolate {
    box_id: u64,
    metadata_file_path: String,
}

#[derive(serde::Serialize)]
pub struct StageResult {
    pub memory: Option<u32>,
    pub exit_code: Option<u32>,
    pub exit_signal: Option<u32>,
    pub exit_message: Option<String>,
    pub exit_status: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub cpu_time: Option<u32>,
    pub wall_time: Option<u32>,
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

impl Isolate {
    pub async fn init(box_id: u64) -> Result<Isolate, Error> {
        let res = Command::new("isolate")
            .args(["--init", "--cg", &format!("-b{}", box_id)])
            .output()
            .await
            .map_err(|e| anyhow!("Failed to get `isolate --init` output\nError: {e}"))?;
        if !res.status.success() {
            return Err(anyhow!(
                "`isolate --init` failed with\nstderr: {}\nstdout: {}",
                String::from_utf8_lossy(&res.stderr),
                String::from_utf8_lossy(&res.stdout),
            ));
        }

        let box_dir = String::from_utf8_lossy(&res.stdout).trim().to_string();
        let metadata_file_path = format!("{box_dir}/metadata.txt");
        Ok(Isolate {
            box_id,
            metadata_file_path,
        })
    }

    pub async fn run(
        &self,
        mounts: &[&str],
        limits: &MandatoryLimits,
        cmd_args: &[String],
    ) -> Result<StageResult, Error> {
        let mut cmd = Command::new("isolate");
        cmd.arg("--run")
            .arg(&format!("--meta={}", self.metadata_file_path))
            .arg("--cg");

        for dir in mounts {
            cmd.arg(format!("--dir={}", dir));
        }

        cmd.arg(format!("--cg-mem={}", limits.memory))
            .arg(format!("--wall-time={}", limits.wall_time))
            .arg(format!("--time={}", limits.cpu_time))
            .arg(format!("--extra-time={}", limits.extra_time))
            .arg(format!("--open-files={}", limits.max_open_files))
            .arg(format!("--fsize={}", limits.max_file_size))
            .arg(format!("--processes={}", limits.max_number_of_processes))
            .arg(format!("-b{}", self.box_id))
            .arg("--")
            .args(cmd_args);

        let cmd_res = cmd
            .output()
            .await
            .map_err(|e| anyhow!("Failed to get `isolate --run` output\nError: {e}"))?;

        let mut memory: Option<u32> = None;
        let mut exit_code: Option<u32> = None;
        let mut exit_signal: Option<u32> = None;
        let mut exit_message: Option<String> = None;
        let mut exit_status: Option<String> = None;
        let mut cpu_time: Option<u32> = None;
        let mut wall_time: Option<u32> = None;

        let metadata_str = fs::read_to_string(&self.metadata_file_path)
            .await
            .map_err(|e| anyhow!("Error reading metadata file: {e}"))?;
        let metadata_lines = metadata_str.lines();
        for line in metadata_lines {
            let (key_res, value_res) = split_metadata_line(line);
            let key =
                key_res.map_err(|_| anyhow!("Failed to parse metadata file, received: {line}"))?;
            let value = value_res
                .map_err(|_| anyhow!("Failed to parse metadata file, received: {line}"))?;
            match key {
                "cgmem" => {
                    memory = Some(value.parse().map_err(|_| {
                        anyhow!("Failed to parse memory usage, received value: {value}")
                    })?)
                }
                "exitcode" => {
                    exit_code = Some(value.parse().map_err(|_| {
                        anyhow!("Failed to parse exit code, received value: {value}")
                    })?)
                }
                "exitsig" => {
                    exit_signal = Some(value.parse().map_err(|_| {
                        anyhow!("Failed to parse exit signal, received value: {value}")
                    })?)
                }
                "message" => exit_message = Some(value.to_string()),
                "status" => exit_status = Some(value.to_string()),
                "time" => {
                    cpu_time = Some(value.parse().map_err(|_| {
                        anyhow!("Failed to parse cpu time, received value: {value}")
                    })?)
                }
                "time-wall" => {
                    wall_time = Some(value.parse().map_err(|_| {
                        anyhow!("Failed to parse wall time, received value: {value}")
                    })?)
                }
                _ => {}
            }
        }

        // Might be an error in the actual isolate command
        if !cmd_res.status.success() && exit_code.is_none() {
            return Err(anyhow!(
                "isolate --run exited with error code but exit code was not found in metadata"
            ));
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

        Ok(result)
    }
}

impl Drop for Isolate {
    fn drop(&mut self) {
        let box_id = self.box_id;
        tokio::spawn(async move {
            let res = Command::new("isolate")
                .args(["--cleanup", "--cg", &format!("-b{}", box_id)])
                .output()
                .await;
            match res {
                Ok(res) => {
                    if !res.status.success() {
                        eprintln!(
                            "`isolate --cleanup` failed with\nstderr: {}\nstdout: {}",
                            String::from_utf8_lossy(&res.stderr),
                            String::from_utf8_lossy(&res.stdout)
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Failed to run `isolate --cleanup`\nError: {e}");
                }
            }
        });
    }
}
