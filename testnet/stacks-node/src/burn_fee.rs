use std::fs;

pub fn read_burn_fee() -> u64 {

    let filename = "~/stx-scripts/config/burn_fee.txt";
    println!("BURN-FEE: In read_burn_fee, fileName: {}", filename);

    let contents = fs::read_to_string(filename)
        .expect("Something went wrong reading the file");

    println!("BURN-FEE: In read_burn_fee, text: {}", contents);

    let burn_fee: u64 = contents.trim().parse().expect("Please type a number!");

    burn_fee
}
