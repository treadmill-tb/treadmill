fn main() {
    // Always include the latest set of database migrations in the binary:
    println!("cargo:rerun-if-changed=migrations");
}
