mod theme;

use slint_keyos_platform::app_ui;

app_ui!("prime-pdf-viewer");

fn app_main(_cx: AppContext, ui: AppWindow) {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    theme::init(&ui);

    // Setup button callback
    ui.global::<Callbacks>().on_button_clicked(move || {
        log::info!("Button clicked!");
    });

    ui.run().expect("UI running");
}
