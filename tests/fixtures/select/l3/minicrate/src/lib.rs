pub mod a;
pub mod b;

#[cfg(test)]
mod tests {
    #[test]
    fn lib_unit() {
        assert_eq!(super::a::fa(), 1);
        let pattern = std::env::var("LLVM_PROFILE_FILE").expect("LLVM_PROFILE_FILE");
        let path = std::path::Path::new(&pattern);
        let name = path.file_name().and_then(|n| n.to_str()).expect("name");
        let index = name.split('-').next().expect("index");
        let dir = path.parent().expect("dir");
        std::fs::write(dir.join(format!("{index}.mark")), "src/lib.rs\nsrc/a.rs\n").expect("mark");
    }
}
