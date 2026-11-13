mod app;
mod fleet_source;
mod refresh;
mod socket_source;
mod theme;
mod view;

#[cfg(test)]
mod golden;

fn main() {
    app::run();
}
