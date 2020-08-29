#![allow(unused_variables)]
#![allow(non_upper_case_globals)]
#![allow(warnings)]
use pbr::ProgressBar;
use bio::io::fasta;
use threadpool::ThreadPool;
use std::io::stderr;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use petgraph_graphml::GraphMl;
use petgraph::graph::DiGraph;
use petgraph::graph::NodeIndex;
use std::iter::FromIterator;
use crate::kmer_vec::get;
use std::collections::HashSet;
extern crate array_tool;
//use adler32::RollingAdler32;
use std::fs;
use crossbeam_utils::thread;
use structopt::StructOpt;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use strsim::levenshtein;
use rayon::prelude::*;
use editdistancewf as wf;
mod utils;
mod banded;
mod gfa_output;
mod seq_output;
mod minimizers;
mod ec_reads;
mod kmer_vec;
mod sparse;
mod poa_new;
mod pairwise;
mod buckets;
mod poa;
mod poa_mod;
use std::env;
// mod kmer_array; // not working yet

const revcomp_aware: bool = true; // shouldn't be set to false except for strand-directed data or for debugging

const pairs: bool = false;
//use typenum::{U31,U32}; // for KmerArray
type Kmer = kmer_vec::KmerVec;
type Overlap= kmer_vec::KmerVec;

pub struct Params
{
    l: usize,
    k: usize,
    n: usize,
    t: usize,
    w: usize,
    density :f64,
    size_miniverse: u32,
    average_lmer_count : f64,
    max_lmer_count : u32,
    min_kmer_abundance : usize,
    levenshtein_minimizers : usize,
    correction_threshold: i32,
    distance: usize,
    reference: bool,
    uhs: bool,
}
pub fn dist(s1: &Vec<u64>, s2: &Vec<u64>, params: &Params) -> f64 {
    let s1_set: HashSet<_> = HashSet::from_iter(s1.iter());
    let s2_set: HashSet<_> = HashSet::from_iter(s2.iter());
    let inter: HashSet<_> = s1_set.intersection(&s2_set).collect();
    let union: HashSet<_> = s1_set.union(&s2_set).collect();
    let distance = params.distance;
    match distance {
        0 => {
            return 1.0 - ((inter.len() as f64) / (union.len() as f64))
        }
        1 => {
            return 1.0 - ((inter.len() as f64) / (s1.len() as f64))
        }
        2 => {
            let jaccard = (inter.len() as f64) / (union.len() as f64);
            let mash: f64 = -1.0 * ((2.0 * jaccard) / (1.0 + jaccard)).ln() / params.l as f64;
            return mash
        }
        _ => {
            let jaccard = (inter.len() as f64) / (union.len() as f64);
            let mash: f64 = (-1.0 / params.k as f64) * ((2.0 * jaccard) / (1.0 + jaccard)).ln();
            return mash
        }
    }
}

fn extract_minimizers(seq: &str, params: &Params, int_to_minimizer: &HashMap<u64, String>, minimizer_to_int: &HashMap<String, u64>, mut lmer_counts: &mut HashMap<String, u32>, uhs_kmers: &HashMap<String, u32>) -> (Vec<String>, Vec<u32>, Vec<u64>)
{
    if params.uhs {
        return minimizers::minhash_uhs(seq.to_string(), params, int_to_minimizer, &mut lmer_counts, uhs_kmers)
    }
    if params.w != 0 {
        return minimizers::minhash_window(seq.to_string(), params, int_to_minimizer, minimizer_to_int, &mut lmer_counts)
    }
    minimizers::minhash(seq.to_string(), params, int_to_minimizer, &mut lmer_counts)
        //wk_minimizers(seq, density) // unfinished
}


fn debug_output_read_minimizers(seq_str: &String, read_minimizers : &Vec<String>, read_minimizers_pos :&Vec<u32>)
{
    println!("\nseq: {}",seq_str);
    print!("min: ");
    let mut current_minimizer :String = "".to_string();
    for i in 0..seq_str.len()
    {
        if read_minimizers_pos.contains(&(i as u32))
        {
            let index = read_minimizers_pos.iter().position(|&r| r == i as u32).unwrap();
            current_minimizer = read_minimizers[index].clone();
            let c = current_minimizer.remove(0);
            if c == seq_str.chars().nth(i).unwrap()
            {
                print!("X");
            }
                else
            {
                print!("x");
            }
            continue;
        }
        if current_minimizer.len() > 0
        {
            let c = current_minimizer.remove(0);
            print!("{}",c);
        }
        else
        {
            print!(".");
        }
    }
    println!("");

}

// here, keep in mind a kmer is in minimizer-space, not base-space
// this code presupposes that the read has already been transformed into a sequence of minimizers
// so it just performs revcomp normalization and solidy check
fn get_seq(int_to_minimizer : &HashMap<u64, String>, kmer_pos: &mut HashMap<Kmer, Vec<u32>>, read_transformed: &Vec<u64>, kmer_seqs_prev: &mut HashMap<Kmer,String>, params: &Params) -> (String, Vec<String>, Vec<u32>, u32, u32)
{
    let k = params.k;
    let l = params.l;
    let n = params.n;
    let min_kmer_abundance = params.min_kmer_abundance;
    let levenshtein_minimizers = params.levenshtein_minimizers;
    let mut read_minimizers = Vec::<String>::new();
    let mut read_minimizers_pos = Vec::<u32>::new();
    let mut seq_str = String::new();
    let mut pos_idx = 0;
    let mut recovered_seqs = 0;
    let mut inserted_seqs = 0;
    for i in 0..(read_transformed.len()-k+1)
    {
        let mut node : Kmer = Kmer::make_from(&read_transformed[i..i+k]);
        let mut kmer = get(&node);
        let mut seq_reversed = false;
        if revcomp_aware { 
            let (node_norm, reversed) = node.normalize(); 
            node = node_norm;
            seq_reversed = reversed;
        }
        if kmer_seqs_prev.contains_key(&node) {
            if seq_reversed {
                seq_str.push_str(&utils::revcomp(&kmer_seqs_prev[&node]));
            }
            else {
                seq_str.push_str(&kmer_seqs_prev[&node])
            }
            kmer.iter().map(|minim| int_to_minimizer[minim].to_string()).collect::<Vec<String>>().iter().for_each(|x| read_minimizers.push(x.to_string()));
            let pos_vec_org = kmer_pos[&node].to_vec();
            //println!("Recovered positions {:?}", pos_vec_org);
            let offset = &kmer_seqs_prev[&node].len();
            let mut pos_vec = pos_vec_org.iter().map(|x| x+pos_idx as u32).collect::<Vec<u32>>();
            pos_vec.iter().for_each(|x| read_minimizers_pos.push(*x));
            pos_idx += offset;
            recovered_seqs += 1;
        }
        else {
            let mut seq = String::new();
            //println!("Not recovered");
            for j in 0..k {
                let min = int_to_minimizer[&kmer[j]].to_string();
                read_minimizers.push(min.to_string());
                read_minimizers_pos.push(pos_idx as u32);
                seq.push_str(&min);
                //seq.push_str("N");
                pos_idx += l;
            }
            //if seq_reversed {seq = utils::revcomp(&seq);}
            seq_str.push_str(&seq);
            inserted_seqs += 1;

        }
    }
    (seq_str, read_minimizers, read_minimizers_pos, recovered_seqs, inserted_seqs)
}
fn read_to_kmers(kmer_origin: &mut HashMap<Kmer,String>, seq_id: &str, corr : bool, kmer_pos: &mut HashMap<Kmer, Vec<u32>>, seq_str :&str, read_transformed: &Vec<u64>, read_minimizers: &Vec<String>, read_minimizers_pos: &Vec<u32>, dbg_nodes: &mut HashMap<Kmer,u32> , kmer_seqs: &mut HashMap<Kmer,String>, minim_shift : &mut HashMap<Kmer, (u32, u32)>, params: &Params)
{
    let k = params.k;
    let l = params.l;
    let n = params.n;
    let min_kmer_abundance = params.min_kmer_abundance;
    let levenshtein_minimizers = params.levenshtein_minimizers;
    for i in 0..(read_transformed.len()-k+1)
    {
        //println!("{} {} {}", read_minimizers.len(), read_minimizers_pos.len(), read_transformed.len());

        let mut node : Kmer = Kmer::make_from(&read_transformed[i..i+k]);
        let mut seq_reversed = false;
        if revcomp_aware { 
            let (node_norm, reversed) = node.normalize(); 
            node = node_norm;
            seq_reversed = reversed;
        } 
        let entry = dbg_nodes.entry(node.clone()).or_insert(0);
        if *entry < 2 {
            *entry += 1;
        }
        
       // let s = node.print_as_string();
        //println!("{:?}", s);
        //if node == Kmer::make_from(&vec![1948, 64, 943, 3497, 2263]).normalize().0 {
        //    println!("{}", seq_str);
        //    println!("{:?}, {:?}, {:?}", read_minimizers, read_minimizers_pos, read_transformed);
        //    assert_eq!(0, 1);
        //}

        // decide if that kmer is finally solid 

        if *entry == min_kmer_abundance as u32 {
            // record sequences associated to solid kmers
            let mut seq = seq_str[read_minimizers_pos[i] as usize..(read_minimizers_pos[i+k-1] as usize + l)].to_string();
            if seq_reversed {
                seq = utils::revcomp(&seq);
            }
               
            kmer_seqs.insert(node.clone(), seq.clone());
            let origin = format!("{}_{}_{}", seq_id, read_minimizers_pos[i].to_string(), read_minimizers_pos[i+k-1].to_string());
            kmer_origin.insert(node.clone(), origin);
            if !corr {
                let mut pos_vec = read_minimizers_pos[i..i+k].to_vec();
                pos_vec = pos_vec.iter().map(|x| x-read_minimizers_pos[i]).collect();
                kmer_pos.insert(node.clone(), pos_vec.clone());
            }
            let position_of_second_minimizer = match seq_reversed {
                true => read_minimizers_pos[i+k-1]-read_minimizers_pos[i+k-2],
                false => read_minimizers_pos[i+1]-read_minimizers_pos[i]
            };
            let position_of_second_to_last_minimizer = match seq_reversed {
                true => read_minimizers_pos[i+1]-read_minimizers_pos[i],
                false => read_minimizers_pos[i+k-1]-read_minimizers_pos[i+k-2]
            };

            minim_shift.insert(node.clone(), (position_of_second_minimizer, position_of_second_to_last_minimizer));

            // some sanity checks
            if levenshtein_minimizers == 0
            {
                for minim in &read_minimizers[i..i+k-1]
                {
                    debug_assert!((!&seq.find(minim).is_none()) || (!utils::revcomp(&seq).find(minim).is_none()));
                }
            }
       }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "rust-mhdbg", about = "Original implementation of MinHash de Bruijn graphs")]
struct Opt {
    /// Activate debug mode
    // short and long flags (-d, --debug) will be deduced from the field's name
    #[structopt(short, long)]
    debug: bool,

    /// Input file
    #[structopt(parse(from_os_str))]
    reads: Option<PathBuf>,
    #[structopt(long)]
    uhs: Option<String>,

    /// Output graph/sequences prefix 
    #[structopt(parse(from_os_str), short, long)]
    prefix: Option<PathBuf>,

    #[structopt(short, long)]
    k: Option<usize>,
    #[structopt(short, long)]
    l: Option<usize>,
    #[structopt(short, long)]
    n: Option<usize>,
    #[structopt(short, long)]
    t: Option<usize>,
    #[structopt(long)]
    density: Option<f64>,
    #[structopt(long)]
    minabund: Option<usize>,
    #[structopt(short, long)]
    w: Option<usize>,
    #[structopt(long)]
    distance: Option<usize>,
    #[structopt(long)]
    correction_threshold: Option<i32>,
    #[structopt(long)]
    levenshtein_minimizers: Option<usize>,
    #[structopt(long)]
    test1: bool,
    #[structopt(long)]
    test2: bool,
    #[structopt(long)]
    no_error_correct: bool,
    #[structopt(long)]
    reference: bool,
}


fn main() {
    let opt = Opt::from_args();      
    let mut uhs : bool = false;
    let mut filename = PathBuf::new();
    let mut uhs_filename = String::new();
    let mut output_prefix;
    let mut k: usize = 10;
    let mut l: usize = 12;
    let mut n: usize = 2;
    let mut t: usize = 0;
    let mut w: usize = 0;
    let mut density :f64 = 0.10;
    let mut min_kmer_abundance: usize = 2;
    let mut levenshtein_minimizers: usize = 0;
    let mut distance: usize = 0;
    let mut error_correct: bool = true;
    let mut correction_threshold : i32 = 0;
    let mut reference : bool = false;
    let mut windowed : bool = false;
    if opt.no_error_correct {
        error_correct = false;
    }
    if opt.reference {
        reference = true;
    }
    if opt.test1 {
        filename = PathBuf::from("../read50x_ref10K_e001.fa"); 
        if opt.k.is_none() { k = 5; }
        if opt.l.is_none() { l = 8; }
        if opt.density.is_none() { density = 1.0 };
    }
    else if opt.test2 {
        filename = PathBuf::from("../SRR9969842_vs_chr4.fasta");
        if opt.k.is_none() { k = 50; }
        if opt.l.is_none() { l = 12; }
        if opt.density.is_none() { density = 0.1 };
    }
    if !opt.k.is_none() { k = opt.k.unwrap() } else { println!("Warning: using default k value ({})",k); } 
    if !opt.l.is_none() { l = opt.l.unwrap() } else { println!("Warning: using default l value ({})",l); }
    if !opt.n.is_none() { n = opt.n.unwrap() } else { println!("Warning: using default n value ({})",n); }
    if !opt.t.is_none() { t = opt.t.unwrap() } else { println!("Warning: using default t value ({})",t); }

    if !opt.density.is_none() { density = opt.density.unwrap() } else { println!("Warning: using default minhash density ({}%)",density*100.0); }
    if !opt.minabund.is_none() { min_kmer_abundance = opt.minabund.unwrap() } else { println!("Warning: using default min kmer abundance value ({})",min_kmer_abundance); }
    if !opt.w.is_none() { windowed = true; w = opt.w.unwrap(); } else { println!("Warning: Using default density-based"); }
    if !opt.correction_threshold.is_none() { correction_threshold = opt.correction_threshold.unwrap() } else { println!("Warning: using default correction threshold value ({})",correction_threshold); }

    if !opt.levenshtein_minimizers.is_none() { levenshtein_minimizers = opt.levenshtein_minimizers.unwrap() }
    if !opt.distance.is_none() { distance = opt.distance.unwrap() }
    if distance > 2 {distance = 2;}
    let distance_type = match distance { 0 => "jaccard", 1 => "containment", 2 => "mash",_ => "mash" };
    let minimizer_type = match levenshtein_minimizers { 0 => "reg", 1 => "lev1", 2 => "lev2",_ => "levX" };
    if opt.levenshtein_minimizers.is_none() { println!("Warning: using default minimizer type ({})",minimizer_type); }
    if opt.distance.is_none() { println!("Warning: using default distance metric ({})",distance_type); }



    output_prefix = PathBuf::from(format!("{}graph-k{}-p{}-l{}",minimizer_type,k,density,l));

    if !opt.reads.is_none() { filename = opt.reads.unwrap().clone(); } 
    if !opt.uhs.is_none() { 
        uhs = true;
        uhs_filename = opt.uhs.unwrap(); 
    } 
    if !opt.prefix.is_none() { output_prefix = opt.prefix.unwrap(); } else { println!("Warning: using default prefix ({})",output_prefix.to_str().unwrap()); }
    
    if filename.as_os_str().is_empty() { panic!("please specify an input file"); }

    let debug = opt.debug;

    let size_miniverse = match revcomp_aware
    {
        false => 4f32.powf(l as f32) as u32,
        true => 4f32.powf(l as f32) as u32 / 2
    };

    let mut params = Params { 
        l,
        k,
        n,
        t,
        w,
        density,
        size_miniverse,
        average_lmer_count: 0.0,
        max_lmer_count: 0,
        min_kmer_abundance,
        levenshtein_minimizers,
        distance,
        correction_threshold,
        reference,
        uhs,
    };
    // init some useful objects
    let mut nb_minimizers_per_read : f64 = 0.0;
    let mut nb_reads : u64 = 0;
    // get file size for progress bar
    let metadata = fs::metadata(&filename).expect("error opening input file");
    let file_size = metadata.len();
    let mut pb = ProgressBar::on(stderr(),file_size);

    let (mut minimizer_to_int, mut int_to_minimizer) = minimizers::minimizers_preparation(&mut params, &filename, file_size, levenshtein_minimizers);
    // fasta parsing
    // possibly swap it later for https://github.com/aseyboldt/fastq-rs
    let reader = fasta::Reader::from_file(&filename).unwrap();

    // create a hash table containing (kmers, count)
    // then will keep only those with count > 1
    let mut dbg_nodes   : HashMap<Kmer,u32> = HashMap::new(); // it's a Counter
    let mut kmer_seqs   : HashMap<Kmer,String> = HashMap::new(); // associate a dBG node to its sequence
    let mut kmer_origin : HashMap<Kmer,String> = HashMap::new(); // remember where in the read/refgenome the kmer comes from, for debugging only
    let mut kmer_seqs_tot : HashMap<Vec<u32>,String> = HashMap::new(); // associate a dBG node to its sequence
    let mut kmer_pos   : HashMap<Kmer,Vec<u32>> = HashMap::new(); // associate a dBG node to its sequence
    let mut minim_shift : HashMap<Kmer,(u32,u32)> = HashMap::new(); // records position of second minimizer in sequence
    let mut seq_mins = Vec::<Vec<u64>>::new();
    let mut read_ids : HashMap<Vec<u64>, String> = HashMap::new();
    let mut pairwise_jaccard = HashMap::new();

    let mut read_to_seq_pos : HashMap<Vec<u64>, (String, Vec<u32>)> = HashMap::new();
    let mut record_len = 0;
    let postcor_path = PathBuf::from(format!("{}.postcor",output_prefix.to_str().unwrap()));
    let mut ec_file         = ec_reads::new_file(&output_prefix);
    let mut ec_file_postcor = ec_reads::new_file(&postcor_path);
    let poa_path = PathBuf::from(format!("{}.poa",output_prefix.to_str().unwrap()));
    let mut ec_file_poa = ec_reads::new_file(&poa_path);
    let mut lmer_counts : HashMap<String, u32> = HashMap::new();
    let mut pairwise_edit : HashMap<(Vec<u64>, Vec<u64>), u32> = HashMap::new();

    let mut buckets : HashMap<Vec<u64>, Vec<Vec<u64>>> = HashMap::new();
    let mut corrected : HashMap<Vec<u64>, (String, Vec<String>, Vec<u32>, Vec<u64>)> = HashMap::new();
    //minimizers::lmer_counting(&mut lmer_counts, &filename, file_size, &mut params);
    let mut uhs_kmers = HashMap::<String, u32>::new();
    if params.uhs {
        uhs_kmers = minimizers::uhs_preparation(&mut params, &uhs_filename)
    }
        for result in reader.records() {
                let record    = result.unwrap();
                let seq_inp       = record.seq();
                let seq_id    = record.id();

                let seq_str = String::from_utf8_lossy(seq_inp);
                let (mut read_minimizers, mut read_minimizers_pos, mut read_transformed) = extract_minimizers(&seq_str, &params, &int_to_minimizer, &minimizer_to_int, &mut lmer_counts, &uhs_kmers);
                read_ids.insert(read_transformed.to_vec(), seq_id.to_string());
                //let (test_min, test_pos, test_trans) = extract_minimizers(&test_str, &params, &lmer_counts, &minimizer_to_int);
                //println!("{:?}", test_trans);
                // stats
                nb_minimizers_per_read += read_transformed.len() as f64;
                nb_reads += 1;
                pb.add(record.seq().len() as u64 + record.id().len() as u64); // get approx size of entry
                record_len = record.id().len();
                // some debug
                //debug_output_read_minimizers(&seq_str.to_string(), &read_minimizers, &read_minimizers_pos);

                //if !error_correct && read_transformed.len() > k {
                   // read_to_kmers(&seq_str, &read_transformed, &read_minimizers, &read_minimizers_pos, &mut dbg_nodes, &mut kmer_seqs, &mut kmer_seqs_tot, &mut seq_mins, &mut minim_shift, &params, false);
                    
               // }
               if read_transformed.len() > k {

               read_to_kmers(&mut kmer_origin, &seq_id, false, &mut kmer_pos, &seq_str, &read_transformed, &read_minimizers, &read_minimizers_pos, &mut dbg_nodes, &mut kmer_seqs, &mut minim_shift, &params);
               ec_reads::record(&mut ec_file, &seq_id, &seq_str, &read_transformed, &read_minimizers, &read_minimizers_pos);

            }
                if error_correct
                {
                    //for i in 0..read_transformed.len()-n+1 {
                    //  let sub_mer = &read_transformed[i..i+n];
                    //  let count = sub_counts.entry(sub_mer.to_vec()).or_insert(0);
                    //   *count += 1;
                    //}
                    if read_transformed.len() >= k {
                        buckets::buckets_insert(read_transformed.to_vec(), params.n, &mut buckets, &mut dbg_nodes);
                        read_to_seq_pos.insert(read_transformed.to_vec(), (seq_str.to_string(), read_minimizers_pos.to_vec()));
                        seq_mins.push(read_transformed.to_vec());

                        //buckets::buckets_insert_base(&seq_str, read_transformed.to_vec(), params.n, &mut buckets_base);
    
                    }
                }

        }
    //buckets = buckets::enumerate_buckets(&mut seq_mins, &mut dbg_nodes, &mut sub_counts, &kmer_seqs_tot, &params);
    
    pb.finish_print("done converting reads to minimizers");
    nb_minimizers_per_read /= nb_reads as f64;
    ec_reads::flush(&mut ec_file);

    println!("avg number of minimizers/read: {}",nb_minimizers_per_read);
    println!("number of transformed reads: {}",seq_mins.len());


    if error_correct
    {
    let mut kmer_seqs_prev = kmer_seqs.clone();
    dbg_nodes = HashMap::new(); // it's a Counter
    kmer_seqs = HashMap::new(); // associate a dBG node to its sequence
    minim_shift = HashMap::new();
    let path = ec_reads::make_filename(&output_prefix); // records position of second minimizer in sequence
    let metadata = fs::metadata(path).expect("error opening ec file");
    let file_size = metadata.len();
    let mut pb = ProgressBar::on(stderr(),file_size);
    let mut rec_seqs = 0;
    let mut ins_seqs = 0;
    kmer_origin = HashMap::new(); // associate a dBG node to its sequence
    let mut pb2 = ProgressBar::on(stderr(),seq_mins.len() as u64);

        // do error correction of reads 
        let mut counter = 1;
        
        for ec_record in ec_reads::load(&output_prefix) 
        {
            let mut seq_id              = ec_record.seq_id;
            let mut read_transformed_org    = ec_record.read_transformed;
            let mut seq_str             = ec_record.seq_str;
            let mut read_minimizers_org = ec_record.read_minimizers;
            let mut read_minimizers_pos_org = ec_record.read_minimizers_pos;
            let mut read_transformed = Vec::<u64>::new();
            let mut seq = String::new();
            let mut read_minimizers = Vec::<String>::new();
            let mut read_minimizers_pos = Vec::<u32>::new();
            //println!("OG:\t{:?}", read_transformed);
            if !corrected.contains_key(&read_transformed_org) {
                let tuple = buckets::query_buckets(&seq_mins, &mut int_to_minimizer, &mut read_to_seq_pos, &mut pairwise_jaccard, &mut ec_file_poa, &mut read_ids, &mut corrected, &read_transformed_org, &mut buckets, &params);
                seq = tuple.0.to_string();
                read_minimizers = tuple.1.to_vec();
                read_minimizers_pos = tuple.2.to_vec();
                read_transformed = tuple.3.to_vec();

                //println!("Cons:\t{:?}", read_transformed);
            }
            else {
                seq = corrected[&read_transformed_org].0.to_string();
                read_minimizers = corrected[&read_transformed_org].1.to_vec();
                read_minimizers_pos = corrected[&read_transformed_org].2.to_vec();
                read_transformed = corrected[&read_transformed_org].3.to_vec();


            }
            //let mut seq = buckets::query_buckets_base(&mut buckets_base, read_transformed, &params);
            //let (read_minimizers, read_minimizers_pos, read_transformed) = extract_minimizers(&seq, &params, &lmer_counts, &minimizer_to_int);
            //let (mut seq, mut read_minimizers, mut read_minimizers_pos, rec, ins) = get_seq(&int_to_minimizer, &mut kmer_pos, &read_transformed, &mut kmer_seqs_prev, &params);
            //rec_seqs += rec;
            //ins_seqs += ins;
            if read_transformed.len() <= k { continue; }
            pb.add(seq_id.len() as u64 + read_transformed.len() as u64 + read_minimizers.len() as u64 + read_minimizers_pos.len() as u64 + seq_str.len() as u64);
            ec_reads::record(&mut ec_file_postcor, &seq_id, &seq, &read_transformed, &read_minimizers, &read_minimizers_pos);
            ec_reads::flush(&mut ec_file_postcor); // flush as we may stop earlier
            ec_reads::flush(&mut ec_file_poa); // flush as we may stop earlier
            read_to_kmers(&mut kmer_origin, &seq_id, true, &mut kmer_pos, &seq, &read_transformed, &read_minimizers, &read_minimizers_pos, &mut dbg_nodes, &mut kmer_seqs, &mut minim_shift, &params);
            //println!("Seq {} done", counter);
 
            // dump corrected reads to [prefix].postcor.ec_data

            counter += 1;
        }
        //println!("{}% of kmers recovered sequences", rec_seqs as f64 / (rec_seqs+ins_seqs) as f64);
    }
    println!("nodes before abund-filter: {}", dbg_nodes.len());
    dbg_nodes.retain(|x,&mut c| c >= (min_kmer_abundance as u32));
    

    

    println!("nodes after: {}", dbg_nodes.len());
    let mut dbg_edges : Vec<(&Kmer,&Kmer)> = Vec::new();

    // index k-1-mers
    let mut km_index : HashMap<Overlap,Vec<&Kmer>> = HashMap::new();
    for node in dbg_nodes.keys()
    {
        let first :Overlap = node.prefix().normalize().0;
        let second  :Overlap = node.suffix().normalize().0;
        let mut insert_km = |key,val| 
        {
            match km_index.entry(key) {
                Entry::Vacant(e) => { e.insert(vec![val]); },
                Entry::Occupied(mut e) => { e.get_mut().push(val); }
            }
        };
        insert_km(first,node);
        insert_km(second,node);
    }

    // create a vector of dbg edges
    for n1 in dbg_nodes.keys() 
    {
        let rev_n1 = n1.reverse();

        /*
           let maybe_insert_edge = |n1 :&[u32;k], n2 :&[u32;k]| {
           if n1[1..] == n2[0..k-1] {
           dbg_edges.push((n1,n2));
           }
           };
           */

        // bit of a rust noob way to code this, because i'm not too familiar with types yet..
        let key1=n1.suffix().normalize().0;
        let key2=n1.prefix().normalize().0;
        for key in [key1,key2].iter()
        {
            if km_index.contains_key(&key)
            {
                let list_of_n2s : &Vec<&Kmer> = km_index.get(&key).unwrap();
                for n2 in list_of_n2s {
                    let rev_n2 = n2.reverse();
                    if n1.suffix() == n2.prefix() {
                        dbg_edges.push((n1,n2));
                    }
                    // I wanted to do this closure, but try it, it doesn't work. A borrowed data problem
                    // apparently.
                    //maybe_insert_edge(n1,n2);
                    if revcomp_aware {
                        if n1.suffix() == rev_n2.prefix()
                            || rev_n1.suffix() == n2.prefix()
                                || rev_n1.suffix() == rev_n2.prefix() {
                                    dbg_edges.push((n1,n2));
                        }

                    }
                }
            }
        }
    }
    println!("edges: {}", dbg_edges.len());

    // create a real bidirected dbg object using petgraph
    let mut gr = DiGraph::<Kmer,Kmer>::new();
    let mut node_indices : HashMap<Kmer,NodeIndex> = HashMap::new(); // bit redundant info, as nodes indices are in order of elements in dbg_nodes already; but maybe don't want to binary search inside it.
    for node in dbg_nodes.keys() { 
        let index = gr.add_node(node.clone());
        node_indices.insert(node.clone(),index);
    }
    let vec_edges : Vec<(NodeIndex,NodeIndex)> = dbg_edges.par_iter().map(|(n1,n2)| (node_indices.get(&n1).unwrap().clone(),node_indices.get(&n2).unwrap().clone())).collect();

    gr.extend_with_edges( vec_edges );

    // graphml output
    if false
        //if true
        {
            let graphml = GraphMl::new(&gr).pretty_print(true);
            std::fs::write("graph.graphml", graphml.to_string()).unwrap();
        }

    // gfa output
    println!("writing GFA..");
    gfa_output::output_gfa(&gr, &dbg_nodes, &output_prefix, &kmer_seqs, &int_to_minimizer, &minim_shift, levenshtein_minimizers);

    // write sequences of minimizers for each node
    // and also read sequences corresponding to those minimizers
    println!("writing sequences..");
    seq_output::write_minimizers_and_seq_of_kmers(&output_prefix, &node_indices, &kmer_seqs, &kmer_origin, &dbg_nodes, k, l);
}
