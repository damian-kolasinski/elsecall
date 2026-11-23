mod add_mac_flow;
mod add_mac_source;
mod add_mac_view;
mod app;
mod attention_view;
mod config_sync_source;
mod config_sync_view;
mod configuration_view;
mod fleet_source;
mod load_dialog;
mod load_trend;
mod provisioning_badge;
mod provisioning_source;
mod provisioning_state;
mod provisioning_view;
mod refresh;
mod release_source;
mod resource_details;
mod resource_display;
mod setup_dialog;
mod socket_source;
mod theme;
mod uninstalled;
mod view;
mod window_upgrade;
mod workload_policy;

#[cfg(test)]
mod golden;

fn main() {
    app::run();
}
