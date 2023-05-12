use std::{
    fs::{self},
    path::PathBuf,
    process::Command,
};

use super::{dockerfile_generation::OutputDir, file_server::FileServerConfig};
use anyhow::{bail, Context, Result};
use std::process::Stdio;
use tracing::info;

const INCREMENTAL_CACHE_DIR: &str = "incremental-cache";
const INCREMENTAL_CACHE_UPLOADS_DIR: &str = "uploads";
const INCREMENTAL_CACHE_IMAGE_DIR: &str = "image";

#[derive(Default)]
pub struct IncrementalCache {}

/// Directories in which to cache Docker image layers.
#[derive(Default)]
pub struct IncrementalCacheDirs {
    out_dir: OutputDir,
    pub uploads_dir: PathBuf,
    pub image_dir: PathBuf,
}

impl IncrementalCacheDirs {
    pub fn new(out_dir: &OutputDir) -> Self {
        let incremental_cache_root = out_dir.get_absolute_path(INCREMENTAL_CACHE_DIR);
        let image_dir = incremental_cache_root.join(PathBuf::from(INCREMENTAL_CACHE_IMAGE_DIR));
        let uploads_dir = incremental_cache_root.join(PathBuf::from(INCREMENTAL_CACHE_UPLOADS_DIR));

        IncrementalCacheDirs {
            out_dir: out_dir.clone(),
            uploads_dir,
            image_dir,
        }
    }

    /// Makes the incremental cache directories.
    pub fn create(&self) -> Result<()> {
        let incremental_cache_root = self.out_dir.get_absolute_path(INCREMENTAL_CACHE_DIR);

        if fs::metadata(&incremental_cache_root)
            .context("Check if incremental-cache dir exists")
            .is_ok()
        {
            fs::remove_dir_all(&incremental_cache_root)?;
        }

        if fs::metadata(&self.image_dir)
            .context("Check if incremental cache image dir exists")
            .is_ok()
        {
            fs::remove_dir_all(&self.image_dir)?;
        }

        if fs::metadata(&self.uploads_dir)
            .context("Check if incremental cache uploads dir exists")
            .is_ok()
        {
            fs::remove_dir_all(&self.uploads_dir)?;
        }

        fs::create_dir_all(&self.image_dir).context("Create incremental cache image dir")?;
        fs::create_dir_all(&self.uploads_dir)
            .context("Creating incremental-cache uploads directory")?;

        Ok(())
    }
}

impl IncrementalCache {
    /// Create a filesystem image for each of the files in the incremental cache uploads directory, then upload these to the Docker cache.
    pub fn create_image(
        &self,
        incremental_cache_dirs: &IncrementalCacheDirs,
        tag: &str,
    ) -> Result<()> {
        let files = fs::read_dir(&incremental_cache_dirs.uploads_dir)?;

        // There are three options to create a filesystem image that contains only tar files
        // #1 Use a Rust crate to create the image: 30+ seconds in a sample test, Also no clear winner Crate for creating OCI image
        // #2 Create minimal Dockerfile: 6 seconds in a sample test
        // #3 Use Docker import: Provide 3 seconds in a sample test
        for f in files {
            let mut docker_import_cmd = Command::new("docker");
            docker_import_cmd.arg("import").arg(&f?.path()).arg(tag);

            let result = docker_import_cmd
                .spawn()?
                .wait()
                .context("Create incremental cache image")?;

            if !result.success() {
                bail!("Creating incremental cache image failed")
            }
        }

        info!("Incremental cache image created: {}", &tag);
        Ok(())
    }

    /// Check if the provided image_tag matches a tag in the incremental Docker image cache.
    pub fn is_image_exists(image_tag: &str) -> Result<bool> {
        let mut docker_inspect_cmd = Command::new("docker");
        docker_inspect_cmd
            .arg("manifest")
            .arg("inspect")
            .arg(image_tag)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let result = docker_inspect_cmd
            .spawn()?
            .wait()
            .context("Check incremental cache image exists in registry")?;

        Ok(result.success())
    }

    /// Produce Dockerfile line(s) copying cached files from the incremental cache to the final build image.
    pub fn get_copy_to_image_command(
        cache_directories: &Option<Vec<String>>,
        incremental_cache_image: &str,
    ) -> Vec<String> {
        let dirs = &cache_directories.clone().unwrap_or_default();
        if dirs.is_empty() {
            return vec![];
        }

        dirs.iter()
            .flat_map(|dir| {
                let target_cache_dir = dir.replace('~', "/root");
                let target_cache_dir_optional = target_cache_dir
                    .split('/')
                    .filter(|c| !c.is_empty())
                    .map(|c| format!("{c}?"))
                    .collect::<Vec<_>>()
                    .join("/");

                vec![format!(
                    "COPY --from={incremental_cache_image} {target_cache_dir_optional} {target_cache_dir}"
                )]
            })
            .collect::<Vec<String>>()
    }

    /// Produce Dockerfile line(s) copying files from the build image into the incremental cache.
    pub fn get_copy_from_image_command(
        cache_directories: &Option<Vec<String>>,
        file_server_config: Option<FileServerConfig>,
    ) -> Vec<String> {
        let container_dirs = cache_directories.clone().unwrap_or_default();
        if container_dirs.is_empty() || file_server_config.is_none() {
            return vec![];
        }

        let server_config = file_server_config.unwrap();
        container_dirs
            .iter()
            .flat_map(|dir| {
                let sanitized_dir = dir.replace('~', "/root");
                let compressed_file_name = format!("{}.tar", sanitized_dir.replace('/', "%2f"));
                vec![
                    format!("if [ -d \"{sanitized_dir}\" ]; then tar -cf {compressed_file_name} {sanitized_dir}; fi;"),
                    format!(
                        "if [ -d \"{sanitized_dir}\" ]; then curl -v -T {} {} --header \"t:{}\" --retry 3 --retry-all-errors; fi;",
                        compressed_file_name, server_config.upload_url, server_config.access_token,
                    ),
                    format!("if [ -d \"{sanitized_dir}\" ]; then rm -rf {sanitized_dir}; fi"),
                ]
            })
            .collect::<Vec<String>>()
    }
}

#[test]
fn test_get_copy_from_image_command() {
    let cmds = IncrementalCache::get_copy_from_image_command(
        &Some(vec!["./parent_dir/child_dir".to_string()]),
        Some(FileServerConfig {
            listen_to_ip: "0.0.0.0".to_string(),
            port: 1234,
            access_token: "test_access_token".to_string(),
            upload_url: "http://test.com/upload".to_string(),
            files_dir: PathBuf::from("./source_dir".to_string()),
        }),
    );

    assert_eq!(cmds.len(), 3);
    assert_eq!(cmds[0], "if [ -d \"./parent_dir/child_dir\" ]; then tar -cf .%2fparent_dir%2fchild_dir.tar ./parent_dir/child_dir; fi;".to_string());
    assert_eq!(cmds[1], "if [ -d \"./parent_dir/child_dir\" ]; then curl -v -T .%2fparent_dir%2fchild_dir.tar http://test.com/upload --header \"t:test_access_token\" --retry 3 --retry-all-errors; fi;".to_string());
    assert_eq!(
        cmds[2],
        "if [ -d \"./parent_dir/child_dir\" ]; then rm -rf ./parent_dir/child_dir; fi".to_string()
    );
}

#[test]
fn test_get_copy_to_image_command() {
    let cmds = IncrementalCache::get_copy_to_image_command(
        &Some(vec!["./parent_dir/child_dir".to_string()]),
        "docker.io/library/test-image",
    );

    assert_eq!(cmds.len(), 1);
    assert_eq!(
        cmds[0],
        "COPY --from=docker.io/library/test-image .?/parent_dir?/child_dir? ./parent_dir/child_dir"
            .to_string()
    );
}
