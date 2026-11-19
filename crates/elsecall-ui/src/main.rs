mod add_mac_flow;
mod add_mac_source;
mod add_mac_view;
mod app;
mod configuration_view;
mod fleet_source;
mod load_trend;
mod refresh;
mod release_source;
mod resource_details;
mod resource_display;
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
