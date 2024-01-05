use crate::console;

pub fn which(console: &mut console::Console, args: &[String]) {
    if args.len() == 0 {
        console.write("which: missing argument");
        return;
    }
    for arg in args.iter() {
        match arg.as_str() {
            "echo" | "which" | "pwd" | "cd" | "set" => {
                console.write(&format!("built-in command: {arg}"));
            },
            _ => (),
        }
    }
}
