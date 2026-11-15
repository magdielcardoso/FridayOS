fn main() {
    let registry = friday_tools::Registry::with_defaults();
    for spec in registry.specs() {
        println!("{:<24} [{}] {}", spec.name, spec.risk.label(), spec.summary);
    }
}
