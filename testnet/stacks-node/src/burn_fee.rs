use std::fs;

pub fn read_burn_fee() -> u64 {

    let fpath = "/home/wit/stx-scripts/config/burn-fee.txt";
    info!("BURN-FEE: In read_burn_fee, fpath: {}", fpath);

    let contents = fs::read_to_string(fpath)
        .expect("Something went wrong reading the file");

    info!("BURN-FEE: In read_burn_fee, text: {}", contents);

    let burn_fee: u64 = contents.trim().parse().expect("Please type a number!");

    burn_fee
}
