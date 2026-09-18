use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui::{self, DragValue, RichText};
use rfd::FileDialog;

struct ScaleResult {
    output: PathBuf,
    stats: brz_scaler::ScaleStats,
}

struct ScalerApp {
    input: String,
    output: String,
    factor: f64,
    axis_factors: [f64; 3],
    use_xyz: bool,
    overwrite: bool,
    running: bool,
    status: String,
    last_output: Option<PathBuf>,
    receiver: Option<Receiver<Result<ScaleResult, String>>>,
}

impl Default for ScalerApp {
    fn default() -> Self {
        Self {
            input: String::new(),
            output: String::new(),
            factor: 2.0,
            axis_factors: [2.0; 3],
            use_xyz: false,
            overwrite: false,
            running: false,
            status: "Copy a BRZ in Explorer, then click Paste BRZ.".into(),
            last_output: None,
            receiver: None,
        }
    }
}

impl ScalerApp {
    fn factors(&self) -> [f64; 3] {
        if self.use_xyz {
            self.axis_factors
        } else {
            [self.factor; 3]
        }
    }

    fn set_input(&mut self, path: PathBuf) {
        self.input = path.to_string_lossy().into_owned();
        self.output = default_output_path(&path, self.factors())
            .to_string_lossy()
            .into_owned();
        self.last_output = None;
        self.overwrite = false;
    }

    fn start(&mut self) {
        let input = PathBuf::from(self.input.trim());
        let output = PathBuf::from(self.output.trim());
        if !input.is_file() {
            self.status = "Input BRZ does not exist.".into();
            return;
        }
        if input
            .extension()
            .and_then(|v| v.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
            != Some("brz")
        {
            self.status = "Input must be a .brz file.".into();
            return;
        }
        if output.exists() && !self.overwrite {
            self.status = "Output exists; enable Allow overwrite or choose another path.".into();
            return;
        }
        let factors = self.factors();
        self.running = true;
        self.last_output = None;
        self.status = format!(
            "Resizing by X={} Y={} Z={}...",
            factors[0], factors[1], factors[2]
        );
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        thread::spawn(move || {
            let options = brz_scaler::ScaleOptions { factors };
            let result = brz_scaler::scale_brz_with_options(&input, &output, options)
                .map(|stats| ScaleResult { output, stats });
            let _ = tx.send(result);
        });
    }

    fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            return;
        };
        self.running = false;
        self.receiver = None;
        match result {
            Ok(result) => {
                let copied = copy_file_to_clipboard(&result.output).is_ok();
                self.status = format!(
                    "Wrote {} bricks; preserved {} entities; skipped {} non-scalable bricks.{}",
                    result.stats.output_bricks,
                    result.stats.preserved_entities,
                    result.stats.skipped_basic_bricks,
                    if copied {
                        " Copied output to clipboard."
                    } else {
                        ""
                    }
                );
                self.last_output = Some(result.output);
            }
            Err(error) => self.status = format!("Failed: {error}"),
        }
    }
}

impl eframe::App for ScalerApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.running {
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::CentralPanel::default().show(context, |ui| {
            ui.heading("Brickadia BRZ Scaler");
            ui.label("Resize procedural bricks, omit non-scalable bricks, and normalize paste placement.");
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                if ui.button("Paste BRZ").clicked() {
                    match files_from_clipboard().and_then(|paths| {
                        paths.into_iter().find(|path| path.extension().and_then(|v| v.to_str()).is_some_and(|v| v.eq_ignore_ascii_case("brz"))).ok_or_else(|| "Clipboard has no BRZ file".into())
                    }) {
                        Ok(path) => self.set_input(path),
                        Err(error) => self.status = error,
                    }
                }
                if ui.button("Browse input...").clicked() {
                    if let Some(path) = FileDialog::new().add_filter("Brickadia BRZ", &["brz"]).pick_file() { self.set_input(path); }
                }
            });
            ui.label("Input");
            ui.text_edit_singleline(&mut self.input);
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Output");
                if ui.button("Browse...").clicked() {
                    let mut dialog = FileDialog::new().add_filter("Brickadia BRZ", &["brz"]);
                    if let Some(parent) = Path::new(&self.output).parent() { dialog = dialog.set_directory(parent); }
                    if let Some(path) = dialog.save_file() { self.output = path.to_string_lossy().into_owned(); }
                }
            });
            ui.text_edit_singleline(&mut self.output);
            ui.add_space(8.0);

            let mut scale_changed = false;
            ui.horizontal(|ui| {
                scale_changed |= ui.radio_value(&mut self.use_xyz, false, "Overall").changed();
                scale_changed |= ui.radio_value(&mut self.use_xyz, true, "Separate X/Y/Z").changed();
                ui.checkbox(&mut self.overwrite, "Allow overwrite");
            });
            if self.use_xyz {
                ui.horizontal(|ui| {
                    for (label, value) in ["X", "Y", "Z"].into_iter().zip(&mut self.axis_factors) {
                        ui.label(label);
                        scale_changed |= ui.add(DragValue::new(value).range(1.0..=1000.0).speed(1.0).fixed_decimals(0)).changed();
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("Scale factor");
                    scale_changed |= ui.add(DragValue::new(&mut self.factor).range(1.0..=1000.0).speed(1.0).fixed_decimals(0)).changed();
                });
            }
            if scale_changed && !self.input.is_empty() {
                if !self.use_xyz {
                    self.axis_factors = [self.factor; 3];
                }
                self.output = default_output_path(Path::new(&self.input), self.factors()).to_string_lossy().into_owned();
                self.overwrite = false;
            }
            ui.label("Scale factors are whole numbers from 1 to 1000. XYZ factors follow build axes.");
            ui.add_space(14.0);

            ui.add_enabled_ui(!self.running, |ui| {
                if ui.button(RichText::new("Scale BRZ").strong()).clicked() { self.start(); }
            });
            if let Some(path) = self.last_output.clone() {
                if ui.button("Copy result to clipboard").clicked() {
                    self.status = match copy_file_to_clipboard(&path) {
                        Ok(()) => "Copied output to clipboard.".into(),
                        Err(error) => error,
                    };
                }
            }
            ui.add_space(12.0);
            ui.label(&self.status);
        });
    }
}

fn default_output_path(input: &Path, factors: [f64; 3]) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|v| v.to_string_lossy())
        .unwrap_or_default();
    let suffix = if factors[0] == factors[1] && factors[1] == factors[2] {
        factors[0].to_string()
    } else {
        format!("x{}_y{}_z{}", factors[0], factors[1], factors[2])
    };
    input.with_file_name(format!("{stem}_scaled_{suffix}.brz"))
}

#[cfg(windows)]
fn files_from_clipboard() -> Result<Vec<PathBuf>, String> {
    clipboard_win::raw::open().map_err(|e| format!("Failed to open clipboard: {e}"))?;
    let mut paths = Vec::new();
    let result = clipboard_win::raw::get_file_list_path(&mut paths)
        .map_err(|e| format!("Clipboard does not contain copied files: {e}"));
    let _ = clipboard_win::raw::close();
    result.map(|_| paths)
}

#[cfg(not(windows))]
fn files_from_clipboard() -> Result<Vec<PathBuf>, String> {
    Err("Clipboard file paste is only available on Windows".into())
}

#[cfg(windows)]
fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    clipboard_win::raw::open().map_err(|e| format!("Failed to open clipboard: {e}"))?;
    let result =
        clipboard_win::raw::set_file_list(&[path]).map_err(|e| format!("Failed to copy file: {e}"));
    let _ = clipboard_win::raw::close();
    result
}

#[cfg(not(windows))]
fn copy_file_to_clipboard(_path: &Path) -> Result<(), String> {
    Err("Clipboard file copy is only available on Windows".into())
}

fn main() -> eframe::Result {
    eframe::run_native(
        "Brickadia BRZ Scaler",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([620.0, 470.0]),
            ..Default::default()
        },
        Box::new(|_| Ok(Box::<ScalerApp>::default())),
    )
}
