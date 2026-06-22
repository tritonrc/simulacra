fn resolve_relative(base_dir: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = if base_dir.is_empty() {
        vec![]
    } else {
        base_dir.split('/').collect()
    };
    for segment in relative.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn main() {
    let base = "/workspace";
    let rel = "../../etc/passwd";
    let resolved = resolve_relative(base, rel);
    println!("Base: '{}', Rel: '{}', Resolved: '{}'", base, rel, resolved);
    
    let base2 = "/a/b";
    let rel2 = "../../../c";
    let resolved2 = resolve_relative(base2, rel2);
    println!("Base: '{}', Rel: '{}', Resolved: '{}'", base2, rel2, resolved2);
}
