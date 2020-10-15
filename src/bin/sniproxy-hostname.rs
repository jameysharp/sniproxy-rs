use std::env;

fn main() {
    if let Some(arg) = env::args().nth(1) {
        let mut hostname = idna::domain_to_ascii_strict(&arg).expect("valid hostname");

        if hostname.ends_with('.') {
            hostname.truncate(hostname.len() - 1);
        }

        #[cfg(feature = "hashed")]
        {
            use blake2::{Blake2s, Digest};
            let hash = Blake2s::digest(hostname.as_bytes());
            hostname = base64::encode_config(&hash, base64::URL_SAFE_NO_PAD);
        }

        println!("{}", hostname);
    } else {
        eprintln!("usage: sniproxy-hostname <hostname>");
        eprintln!("Prints the hostname in the format that sniproxy expects to find. The");
        eprintln!("hostname may be Unicode, in which case it will be encoded to Punycode.");
        #[cfg(feature = "hashed")]
        eprintln!("This build of sniproxy uses hashed hostnames.");
    }
}
