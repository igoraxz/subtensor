use alloc::string::String;
use frame_support::IterableStorageMap;
use frame_support::weights::Weight;

use super::*;

pub fn migrate_root_claim_type_to_keep<T: Config>() -> Weight {
    let migration_name = b"migrate_root_claim_type_to_keep".to_vec();

    // Initialize the weight with one read operation.
    let mut weight = T::DbWeight::get().reads(1);

    // Check if the migration has already run
    if HasMigrationRun::<T>::get(&migration_name) {
        log::info!(
            "Migration '{:?}' has already run. Skipping.",
            String::from_utf8_lossy(&migration_name)
        );
        return weight;
    }
    log::info!(
        "Running migration '{}'",
        String::from_utf8_lossy(&migration_name)
    );

    // Iterate through all RootClaimType entries and convert to Keep
    let mut reads = 0u64;
    let mut writes = 0u64;

    for (coldkey, old_type) in RootClaimType::<T>::iter() {
        reads += 1;
        // Convert any old type to Keep
        // Old variants: Swap, Keep, KeepSubnets { subnets }
        // New variants: Keep, AutoKeep
        // All old types become Keep (default, manual claim only)
        RootClaimType::<T>::insert(coldkey, RootClaimTypeEnum::Keep);
        writes += 1;
    }

    weight = weight.saturating_add(T::DbWeight::get().reads_writes(reads, writes));

    // Mark the migration as completed
    HasMigrationRun::<T>::insert(&migration_name, true);
    weight = weight.saturating_add(T::DbWeight::get().writes(1));

    log::info!(
        "Migration '{:?}' completed. Converted {} root claim types to Keep.",
        String::from_utf8_lossy(&migration_name),
        writes
    );

    // Return the migration weight.
    weight
}

