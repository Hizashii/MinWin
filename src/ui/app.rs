use eframe::egui;

use crate::{benchmark, system};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Scan,
    Optimize,
    Services,
    Startup,
    Benchmark,
}

pub struct MinWinApp {
    current_page: Page,
    status_message: String,
    system_status: system::status::SystemStatus,
}

impl Default for MinWinApp {
    fn default() -> Self {
        Self {
            current_page: Page::Dashboard,
            status_message: "Ready.".to_owned(),
            system_status: system::status::get_system_status(),
        }
    }
}

pub fn run() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([760.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "MinWin Control Center",
        native_options,
        Box::new(|creation_context| {
            creation_context.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(MinWinApp::default()))
        }),
    )
}

impl eframe::App for MinWinApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.header(context);
        self.sidebar(context);
        self.status_bar(context);

        egui::CentralPanel::default().show(context, |ui| {
            ui.add_space(8.0);
            match self.current_page {
                Page::Dashboard => self.dashboard_page(ui),
                Page::Scan => self.scan_page(ui),
                Page::Optimize => self.optimize_page(ui),
                Page::Services => self.services_page(ui),
                Page::Startup => self.startup_page(ui),
                Page::Benchmark => self.benchmark_page(ui),
            }
        });
    }
}

impl MinWinApp {
    fn header(&self, context: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(76.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("MinWin");
                    ui.label(egui::RichText::new("v0.1").weak());
                });
                ui.label("Windows Performance Control Center");
            });
    }

    fn sidebar(&mut self, context: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .exact_width(170.0)
            .resizable(false)
            .show(context, |ui| {
                ui.add_space(12.0);
                for (page, label) in [
                    (Page::Dashboard, "Dashboard"),
                    (Page::Scan, "Scan"),
                    (Page::Optimize, "Optimize"),
                    (Page::Services, "Services"),
                    (Page::Startup, "Startup"),
                    (Page::Benchmark, "Benchmark"),
                ] {
                    if ui
                        .selectable_label(self.current_page == page, label)
                        .clicked()
                    {
                        self.current_page = page;
                    }
                }
            });
    }

    fn status_bar(&self, context: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(28.0)
            .show(context, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(&self.status_message);
                });
            });
    }

    fn dashboard_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("System Status");
        ui.label("Read-only prototype values are marked as placeholders.");
        ui.add_space(12.0);

        ui.horizontal_wrapped(|ui| {
            metric_card(
                ui,
                "RAM",
                format_percent(self.system_status.ram_usage_percent),
            );
            metric_card(
                ui,
                "CPU",
                format_percent(self.system_status.cpu_usage_percent),
            );
            metric_card(
                ui,
                "Processes",
                format_count(self.system_status.process_count),
            );
            metric_card(
                ui,
                "Services",
                format_count(self.system_status.service_count),
            );
            metric_card(
                ui,
                "Uptime",
                self.system_status
                    .uptime
                    .unwrap_or("-- (placeholder)")
                    .to_owned(),
            );
        });

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if ui.button("Scan System").clicked() {
                self.current_page = Page::Scan;
                self.status_message =
                    "System scan placeholder — no system changes made.".to_owned();
            }
            if ui.button("Run Benchmark").clicked() {
                self.current_page = Page::Benchmark;
                self.status_message =
                    "Benchmark placeholder — no measurements collected yet.".to_owned();
            }
        });
    }

    fn scan_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("System Scanner");
        ui.label("MinWin will analyze:");
        ui.add_space(8.0);
        for item in [
            "startup applications",
            "services",
            "scheduled tasks",
            "installed applications",
            "unnecessary background activity",
        ] {
            ui.label(format!("• {item}"));
        }
        ui.add_space(16.0);
        if ui.button("Start Scan").clicked() {
            self.status_message = "System scan placeholder — no system changes made.".to_owned();
        }
    }

    fn optimize_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Optimization Profiles");
        ui.label("Profiles are currently dry runs and do not modify Windows.");
        ui.add_space(12.0);

        for profile in ["Minimal", "Gaming", "Developer"] {
            ui.horizontal(|ui| {
                ui.label(profile);
                if ui.button("Apply").clicked() {
                    self.status_message = "Dry run only — no system changes made.".to_owned();
                }
            });
            ui.add_space(6.0);
        }

        ui.add_space(8.0);
        ui.colored_label(
            egui::Color32::from_rgb(236, 190, 92),
            "Dry run only — no system changes made.",
        );
    }

    fn services_page(&self, ui: &mut egui::Ui) {
        ui.heading("Services");
        ui.label("Placeholder entries until service enumeration is connected.");
        ui.add_space(12.0);

        egui::Grid::new("services_table")
            .striped(true)
            .min_col_width(150.0)
            .show(ui, |ui| {
                ui.strong("Service");
                ui.strong("Status");
                ui.strong("Startup");
                ui.end_row();

                ui.label("ExampleService");
                ui.label("Running");
                ui.label("Automatic");
                ui.end_row();
            });
    }

    fn startup_page(&self, ui: &mut egui::Ui) {
        ui.heading("Startup");
        ui.label("Placeholder entries until startup enumeration is connected.");
        ui.add_space(12.0);

        egui::Grid::new("startup_table")
            .striped(true)
            .min_col_width(180.0)
            .show(ui, |ui| {
                ui.strong("Application");
                ui.strong("Location");
                ui.strong("Status");
                ui.end_row();

                ui.label("ExampleStartupApp");
                ui.label("User startup");
                ui.label("Enabled");
                ui.end_row();
            });
    }

    fn benchmark_page(&self, ui: &mut egui::Ui) {
        let results = benchmark::placeholder_results();

        ui.heading("Benchmark");
        ui.label("Results are placeholders until benchmark collection is implemented.");
        ui.add_space(12.0);

        egui::Grid::new("benchmark_table")
            .striped(true)
            .min_col_width(170.0)
            .show(ui, |ui| {
                benchmark_row(ui, "RAM", results.ram);
                benchmark_row(ui, "CPU", results.cpu);
                benchmark_row(ui, "Processes", results.processes);
                benchmark_row(ui, "Services", results.services);
                benchmark_row(ui, "Boot Time", results.boot_time);
            });

        ui.add_space(16.0);
        if ui.button("Run Benchmark").clicked() {
            // PLACEHOLDER: this will call the benchmark backend once it is ready.
        }
    }
}

fn metric_card(ui: &mut egui::Ui, label: &str, value: String) {
    ui.group(|ui| {
        ui.label(egui::RichText::new(label).weak());
        ui.label(egui::RichText::new(value).size(20.0).strong());
    });
}

fn benchmark_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(label);
    ui.label(value);
    ui.end_row();
}

fn format_percent(value: Option<u8>) -> String {
    value
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "-- (placeholder)".to_owned())
}

fn format_count(value: Option<u32>) -> String {
    value
        .map(|count| count.to_string())
        .unwrap_or_else(|| "-- (placeholder)".to_owned())
}
