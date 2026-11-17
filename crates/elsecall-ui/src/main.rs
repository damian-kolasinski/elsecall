mod airdrop;
mod app;
mod fleet_source;
mod installer_transfer;
mod load_trend;
mod onboarding;
mod onboarding_demo;
mod onboarding_socket;
mod refresh;
mod release_source;
mod socket_source;
mod theme;
mod view;
mod window_upgrade;

#[cfg(test)]
mod golden;

fn main() {
    app::run();
}
