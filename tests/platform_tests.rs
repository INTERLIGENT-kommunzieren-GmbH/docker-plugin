use docker_control::utils::platform;

#[test]
fn test_get_brew_prefix() {
    match platform::get_brew_prefix() {
        Some(prefix) => println!("Brew prefix found: {}", prefix),
        None => println!("Brew prefix not found"),
    }
}
