use anyhow::{Context, Result};
use reqwest::redirect::Policy;
use std::ffi::OsString;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::ZipArchive;

use crate::java::{Java, PATH_SEPARATOR, capture_cmd};

const PRELOADER_JAR: &str = "lib/kotlin-preloader.jar";
const COMPILER_JAR: &str = "lib/kotlin-compiler.jar";
const COMPILER_CLASS: &str = "org.jetbrains.kotlin.cli.jvm.K2JVMCompiler";
const PRELOADER_CLASS: &str = "org.jetbrains.kotlin.preloading.Preloader";

pub struct Kotlin {
    home: PathBuf,
}

impl Kotlin {
    pub async fn new(version: Option<&str>, build_dir: &Path) -> Result<Self> {
        let kotlin_home = std::env::var("KOTLIN_HOME").ok().map(PathBuf::from);

        let home = match (version, kotlin_home) {
            (None, Some(home)) => home,
            (None, None) => anyhow::bail!(
                "kotlin version must be set in the manifest or KOTLIN_HOME must be set"
            ),
            (Some(ver), Some(home)) => {
                verify_version(&home, ver)?;
                home
            }
            (Some(ver), None) => {
                let cached = build_dir.join("kotlin");
                if cached.is_dir() && verify_version(&cached, ver).is_ok() {
                    cached
                } else {
                    let status = crate::status::StatusHandle::get();
                    status.begin("kotlin", format!("downloading kotlinc {ver}"));
                    let result = Self::download(ver, build_dir).await;
                    if result.is_ok() {
                        status.end_log("kotlin", format!("downloaded kotlinc {ver}"));
                    } else {
                        status.end("kotlin");
                    }
                    result?
                }
            }
        };

        Ok(Self { home })
    }

    async fn download(version: &str, build: &Path) -> Result<PathBuf> {
        let url = format!(
            "https://github.com/JetBrains/kotlin/releases/download/v{version}/kotlin-compiler-{version}.zip"
        );

        let client = reqwest::Client::builder()
            .user_agent(format!("borneo/{}", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::limited(1))
            .build()
            .expect("failed to build HTTP client");

        let resp = client
            .get(url)
            .send()
            .await
            .context("failed to download kotlin compiler")?
            .error_for_status()
            .context("failed to download kotlin compiler")?;

        let bytes = resp
            .bytes()
            .await
            .context("failed to download kotlin compiler")?;

        let mut zip = ZipArchive::new(Cursor::new(bytes))
            .context("github returned invalid ZIP file for kotlin compiler")?;

        let out = build.join("kotlin/");
        std::fs::create_dir_all(&out).context("failed to create kotlin cache directory")?;

        for i in 0..zip.len() {
            let mut file = zip.by_index(i).expect("malformed ZIP");
            let name = file
                .enclosed_name()
                .context("kotlin compiler contains illegal file names")?;
            let Ok(name) = name.strip_prefix("kotlinc") else {
                continue;
            };

            let dest = if name.file_name().map(|n| n == "build.txt").unwrap_or(false) {
                out.join("build.txt")
            } else if name.starts_with("lib/") {
                out.join(name)
            } else {
                continue;
            };

            if file.is_dir() {
                std::fs::create_dir_all(dest).unwrap();
            } else {
                let mut dst =
                    std::fs::File::create(dest).context("failed to create destination file")?;
                std::io::copy(&mut file, &mut dst).context("failed to copy to destination file")?;
            }
        }

        Ok(out)
    }

    pub fn kotlinc<'a>(
        &self,
        java: &Java,
        base: &Path,
        out: &Path,
        class_path: impl Iterator<Item = &'a PathBuf>,
        files: &[PathBuf],
        extra_args: &[String],
    ) -> Result<std::process::Output> {
        let preloader = self.home.join(PRELOADER_JAR);
        let compiler = self.home.join(COMPILER_JAR);

        let mut cmd = Command::new(java.bin("java"));
        cmd.current_dir(base);
        cmd.arg("-cp").arg(&preloader);
        cmd.arg(PRELOADER_CLASS);
        cmd.arg("-cp").arg(&compiler);
        cmd.arg(COMPILER_CLASS);

        cmd.arg("-d").arg(out);

        let cp: Vec<_> = class_path.map(|p| p.as_os_str()).collect();
        let cp = cp.join(&OsString::from(PATH_SEPARATOR));
        if !cp.is_empty() {
            cmd.arg("-cp").arg(cp);
        }

        cmd.args(extra_args);
        cmd.args(files);
        capture_cmd(&mut cmd, "kotlinc")
    }
}

pub struct KotlinCompiler {
    kotlin: Kotlin,
    source: PathBuf,
    args: Vec<String>,
}

impl KotlinCompiler {
    pub fn new(kotlin: Kotlin, source: PathBuf, args: Vec<String>) -> Self {
        Self {
            kotlin,
            source,
            args,
        }
    }
}

impl crate::project::Compiler for KotlinCompiler {
    fn name(&self) -> &str {
        "kotlinc"
    }

    fn source(&self) -> &Path {
        &self.source
    }

    fn compile(
        &self,
        project: &crate::project::Project,
        out: &Path,
        files: &[PathBuf],
    ) -> Result<std::process::Output> {
        self.kotlin.kotlinc(
            project.java(),
            &project.dir,
            out,
            project.class_path_iter(),
            files,
            &self.args,
        )
    }
}

fn read_version(home: &Path) -> Option<String> {
    let content = std::fs::read_to_string(home.join("build.txt")).ok()?;
    content
        .split_whitespace()
        .next()
        .and_then(|s| s.split('-').next())
        .map(str::to_string)
}

fn verify_version(home: &Path, expected: &str) -> Result<()> {
    match read_version(home) {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => anyhow::bail!(
            "kotlin version mismatch: expected {expected}, found {actual} at {}",
            home.display()
        ),
        None => anyhow::bail!("cannot determine kotlin version at {}", home.display()),
    }
}
