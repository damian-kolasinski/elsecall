mod app;
mod fleet_source;
mod load_trend;
mod refresh;
mod release_source;
mod socket_source;
mod theme;
mod uninstalled;
mod view;
mod window_upgrade;

#[cfg(test)]
mod golden;

fn main() {
    app::run();
}
