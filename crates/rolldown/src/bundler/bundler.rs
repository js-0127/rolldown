use std::sync::Arc;

use rolldown_error::BuildError;
use rolldown_fs::{FileSystem, OsFileSystem};
use rolldown_resolver::Resolver;
use sugar_path::AsPath;

use super::{
  bundle::output::Output,
  options::input_options::SharedInputOptions,
  plugin_driver::{PluginDriver, SharedPluginDriver},
  stages::link_stage::{LinkStage, LinkStageOutput},
};
use crate::{
  bundler::stages::{bundle_stage::BundleStage, scan_stage::ScanStage},
  error::BatchedResult,
  plugin::plugin::BoxPlugin,
  HookBuildEndArgs, InputOptions, OutputOptions, SharedResolver,
};

// Rolldown use this alias for outside users.
type BuildResult<T> = Result<T, Vec<BuildError>>;

pub struct RolldownOutput {
  pub warnings: Vec<BuildError>,
  pub assets: Vec<Output>,
}

pub struct Bundler<T: FileSystem + Default> {
  input_options: SharedInputOptions,
  plugin_driver: SharedPluginDriver,
  fs: T,
  resolver: SharedResolver<T>,
  // Store the build result, using for generate/write.
  build_result: Option<LinkStageOutput>,
}

impl Bundler<OsFileSystem> {
  pub fn new(input_options: InputOptions) -> Self {
    Self::with_plugins(input_options, vec![])
  }

  pub fn with_plugins(input_options: InputOptions, plugins: Vec<BoxPlugin>) -> Self {
    Self::with_plugins_and_fs(input_options, plugins, OsFileSystem)
  }
}

impl<T: FileSystem + Default + 'static> Bundler<T> {
  pub fn with_plugins_and_fs(input_options: InputOptions, plugins: Vec<BoxPlugin>, fs: T) -> Self {
    rolldown_tracing::try_init_tracing();
    Self {
      resolver: Resolver::with_cwd_and_fs(input_options.cwd.clone(), false, fs.share()).into(),
      plugin_driver: Arc::new(PluginDriver::new(plugins)),
      input_options: Arc::new(input_options),
      fs,
      build_result: None,
    }
  }

  pub async fn write(&mut self, output_options: OutputOptions) -> BuildResult<RolldownOutput> {
    let dir =
      self.input_options.cwd.as_path().join(&output_options.dir).to_string_lossy().to_string();

    let output = self.bundle_up(output_options).await?;

    self.fs.create_dir_all(dir.as_path()).unwrap_or_else(|_| {
      panic!(
        "Could not create directory for output chunks: {:?} \ncwd: {}",
        dir.as_path(),
        self.input_options.cwd.display()
      )
    });
    for chunk in &output.assets {
      let dest = dir.as_path().join(chunk.file_name());
      if let Some(p) = dest.parent() {
        if !self.fs.exists(p) {
          self.fs.create_dir_all(p).unwrap();
        }
      };
      self.fs.write(dest.as_path(), chunk.content().as_bytes()).unwrap_or_else(|_| {
        panic!("Failed to write file in {:?}", dir.as_path().join(chunk.file_name()))
      });
    }

    Ok(output)
  }

  pub async fn generate(&mut self, output_options: OutputOptions) -> BuildResult<RolldownOutput> {
    self.bundle_up(output_options).await
  }

  pub async fn build(&mut self) -> BuildResult<()> {
    self.build_inner().await?;
    Ok(())
  }

  async fn build_inner(&mut self) -> BatchedResult<()> {
    self.plugin_driver.build_start().await?;

    let build_ret = self.try_build().await;

    if let Err(e) = build_ret {
      let error = e.get().expect("should have a error");
      self
        .plugin_driver
        .build_end(Some(&HookBuildEndArgs {
          // TODO(hyf0): 1.Need a better way to expose the error
          error: error.to_string(),
        }))
        .await?;
      return Err(e);
    }

    self.plugin_driver.build_end(None).await?;
    self.build_result = build_ret.ok();
    Ok(())
  }

  #[tracing::instrument(skip_all)]
  async fn try_build(&mut self) -> BatchedResult<LinkStageOutput> {
    let build_info = ScanStage::new(
      Arc::clone(&self.input_options),
      Arc::clone(&self.plugin_driver),
      self.fs.share(),
      Arc::clone(&self.resolver),
    )
    .scan()
    .await?;

    let link_stage = LinkStage::new(build_info);

    Ok(link_stage.link())
  }

  #[tracing::instrument(skip_all)]
  async fn bundle_up(&mut self, output_options: OutputOptions) -> BuildResult<RolldownOutput> {
    tracing::trace!("InputOptions {:#?}", self.input_options);
    tracing::trace!("OutputOptions: {output_options:#?}",);
    let graph = self.build_result.as_mut().expect("Build should success");
    let mut bundle_stage = BundleStage::new(graph, &self.input_options, &output_options);
    let assets = bundle_stage.bundle();

    Ok(RolldownOutput { warnings: std::mem::take(&mut graph.warnings), assets })
  }
}
