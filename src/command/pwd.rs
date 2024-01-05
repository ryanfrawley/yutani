use crate::console;

pub fn pwd(console: &mut console::Console, args: &[String]) {
    if args.len() > 0 {
        console.write("pwd: invalid arguments");
    } else {
        console.write(&console.current_dir.clone());
    }
}
