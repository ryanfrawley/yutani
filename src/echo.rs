use super::console;

pub fn echo(console: &mut console::Console, args: &[String]) {
    let mut first = true;
    for arg in args.iter() {
        if first {
            first = false;
        } else {
            console.write(" ");
        }
        console.write(arg);
    }
}
