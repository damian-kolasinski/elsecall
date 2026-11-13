//! Identity of active, public scheduling settings, independent of document form,
//! sync generation, service start time and transient machine load.
use super::{capacity::Capacity, parameters::Parameters};

pub fn fingerprint(parameters: &Parameters, capacity: Capacity) -> String {
    let bytes = serde_json::to_vec(&("elsecall/scheduling-settings/v1", parameters, capacity))
        .expect("scheduling settings serialize");
    crate::build_identity::Digest::of(&bytes)
        .build_id()
        .as_str()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{inspection::CoreCounts, scheduling::capacity::Cores};

    #[test]
    fn identity_covers_parameters_capacity_and_reserve() {
        let parameters = Parameters::default();
        let capacity = Capacity::of(
            CoreCounts {
                performance: 8,
                efficiency: 0,
            },
            Cores::ONE,
        );
        let before = fingerprint(&parameters, capacity);
        assert_eq!(before, fingerprint(&parameters.clone(), capacity));
        let mut changed = parameters.clone();
        changed.placement.keep_local_margin += 0.1;
        assert_ne!(before, fingerprint(&changed, capacity));
        for changed in [
            Capacity {
                reserved: Cores::ZERO,
                ..capacity
            },
            Capacity {
                weighted_cores: Cores::ONE,
                ..capacity
            },
            Capacity {
                capacity: Cores::ZERO,
                ..capacity
            },
        ] {
            assert_ne!(before, fingerprint(&parameters, changed));
        }
    }
}
