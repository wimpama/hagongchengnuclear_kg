fn main() {
    // SQLx embeds migrations in the binary. Include newly added migration files
    // in Cargo's change detection as well as changes to existing SQL files.
    println!("cargo:rerun-if-changed=migrations");
}
