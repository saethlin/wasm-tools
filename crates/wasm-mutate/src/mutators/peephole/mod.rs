use egg::{
    define_language, rewrite, Analysis, CostFunction, EClass, EGraph, Id, Language, Pattern,
    RecExpr, Rewrite, Runner, Searcher, Symbol,
};
use rand::{
    prelude::{IteratorRandom, SliceRandom, SmallRng},
    Rng,
};
use std::{cmp::Ordering, collections::HashMap, hash::Hash, num::Wrapping};
use wasm_encoder::{CodeSection, Function, Instruction, Module, ValType};
use wasmparser::{BinaryReaderError, CodeSectionReader, FunctionBody, Operator, Range};

use crate::{
    module::{map_operator, map_type},
    ModuleInfo, Result, WasmMutate,
};

use super::Mutator;

pub struct PeepholeMutator;

// egg language definition

define_language! {
    enum Lang {
        "i32.add" = I32Add([Id; 2]),
        "i32.sub" = I32Sub([Id; 2]),
        "i32.mul" = I32Mul([Id; 2]),
        // "i32.div_s" = I32DivS([Id; 2]),
        // "i32.div_u" = I32DivU([Id; 2]),
        // "i32.rem_s" = I32RemS([Id; 2]),
        // "i32.rem_u" = I32RemU([Id; 2]),
        "i32.and" = I32And([Id; 2]),
        "i32.or" = I32Or([Id; 2]),
        "i32.xor" = I32Xor([Id; 2]),
        "i32.shl" = I32Shl([Id; 2]),
        // "i32.shr_s" = I32ShrS([Id; 2]),
        "i32.shr_u" = I32ShrU([Id; 2]),

        // "i32.rotl" = I32Rotl([Id; 2]),
        // "i32.rotr" = I32Rotr([Id; 2]),
        // "i32.clz" = I32Clz(Id),
        // "i32.ctz" = I32Ctz(Id),
        "i32.popcnt" = I32Popcnt(Id),

        "rand" = Rand, // This operation represent a random number, if its used, every time is should represent the same random number
        // Custom mutators
        "unfold" = Unfold([Id; 1]),
        I32Const(i32),

        // NB: must be last since variants are parsed in order.
        Symbol(Symbol),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PeepholeMutationAnalysis;

impl Analysis<Lang> for PeepholeMutationAnalysis {
    type Data = Option<i32>;

    fn make(egraph: &EGraph<Lang, Self>, enode: &Lang) -> Self::Data {
        None
    }

    fn merge(&self, to: &mut Self::Data, from: Self::Data) -> bool {
        egg::merge_if_different(to, to.or(from))
    }

    fn modify(egraph: &mut EGraph<Lang, Self>, id: Id) {
        if let Some(x) = egraph[id].data {
            let added = egraph.add(Lang::I32Const(x));
            egraph.union(id, added);
        }
    }
}

pub struct NoPopcnt;

impl CostFunction<Lang> for NoPopcnt {
    type Cost = usize;
    fn cost<C>(&mut self, enode: &Lang, mut costs: C) -> Self::Cost
    where
        C: FnMut(Id) -> Self::Cost,
    {
        let op_cost = match enode {
            Lang::I32Popcnt(_) => usize::MAX,
            _ => 0,
        };
        enode.fold(op_cost, |sum, id| sum.saturating_add(costs(id)))
    }
}

// Code mutator, function id, operator id
type MutationContext = (Function, u32);
// Helper type to return operator and ofsset inside the byte stream
type TupleType<'a> = (Operator<'a>, usize);
impl PeepholeMutator {

    fn find_costs<CF>(
        egraph: &EGraph<Lang, PeepholeMutationAnalysis>,
        mut cf: CF,
    ) -> HashMap<Id, (CF::Cost, usize)>
    where
        CF: CostFunction<Lang>,
    {
        let mut costs = HashMap::new();

        let mut did_something = true;
        while did_something {
            did_something = false;

            for class in egraph.classes() {
                let pass = PeepholeMutator::make_pass(egraph, &mut cf, &mut costs, class);
                match (costs.get(&class.id), pass) {
                    (None, Some(new)) => {
                        costs.insert(class.id, new);
                        did_something = true;
                    }
                    (Some(old), Some(new)) if new.0 < old.0 => {
                        costs.insert(class.id, new);
                        did_something = true;
                    }
                    _ => (),
                }
            }
        }

        costs
    }

    fn make_pass<CF>(
        egraph: &EGraph<Lang, PeepholeMutationAnalysis>,
        cf: &mut CF,
        costs: &mut HashMap<Id, (CF::Cost, usize)>,
        eclass: &EClass<Lang, Option<i32>>,
    ) -> Option<(CF::Cost, usize)>
    where
        CF: CostFunction<Lang>,
    {
        let (cost, node_idx) = eclass
            .iter()
            .enumerate()
            .map(|(i, n)| (PeepholeMutator::node_total_cost(egraph, cf, costs, n), i))
            .min_by(|a, b| PeepholeMutator::cmp(&a.0, &b.0))
            .unwrap_or_else(|| panic!("Can't extract, eclass is empty: {:#?}", eclass));
        cost.map(|c| (c, node_idx))
    }

    fn node_total_cost<CF>(
        egraph: &EGraph<Lang, PeepholeMutationAnalysis>,
        cf: &mut CF,
        costs: &mut HashMap<Id, (CF::Cost, usize)>,
        node: &Lang,
    ) -> Option<CF::Cost>
    where
        CF: CostFunction<Lang>,
    {
        let has_cost = |&id| costs.contains_key(&egraph.find(id));
        if node.children().iter().all(has_cost) {
            let costs = &costs;
            let cost_f = |id| costs[&egraph.find(id)].0.clone();
            Some(cf.cost(&node, cost_f))
        } else {
            None
        }
    }

    fn cmp<T: PartialOrd>(a: &Option<T>, b: &Option<T>) -> Ordering {
        // None is high
        match (a, b) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap(),
        }
    }

    // Map operator to Lang expr and the corresponding instruction
    // This method should return the expression (egg language) expected from the operator
    // It would be good here to also have CFG information
    fn operator2term(
        &self,
        operators: &Vec<TupleType>,
        at: usize,
    ) -> Option<(&str, HashMap<&str, Range>, usize)> {
        let (op, _) = &operators[at];
        match op {
            Operator::I32Const { .. } => Some((
                "?x",
                [(
                    "?x",
                    Range {
                        start: at,
                        end: at + 1,
                    },
                )]
                .iter()
                .cloned()
                .collect(),
                at,
            )),
            Operator::I32Shl => {
                let (previous2, _) = &operators[at - 2];
                let (previous, _) = &operators[at - 1];
                if let Operator::I32Const { value } = previous2 {
                    if let Operator::I32Const { value } = previous {
                        match value {
                            1 => {
                                return Some((
                                    "(i32.shl ?x 1)",
                                    [(
                                        "?x",
                                        Range {
                                            start: at - 2,
                                            end: at - 1,
                                        },
                                    )]
                                    .iter()
                                    .cloned()
                                    .collect(),
                                    at - 2,
                                ))
                            }
                            _ => return None,
                        }
                    }
                }

                None
            }
            _ => None,
        }
    }

    fn lang2wasm<'a>(l: &Lang) -> Result<Instruction<'a>> {
        println!("{:?}", l);

        match l {
            Lang::I32Add(_) => Ok(Instruction::I32Add),
            Lang::I32Sub(_) => Ok(Instruction::I32Sub),
            Lang::I32Mul(_) => Ok(Instruction::I32Mul),
            Lang::I32And(_) => Ok(Instruction::I32And),
            Lang::I32Or(_) => Ok(Instruction::I32Or),
            Lang::I32Xor(_) => Ok(Instruction::I32Xor),
            Lang::I32Shl(_) => Ok(Instruction::I32Shl),
            Lang::I32ShrU(_) => Ok(Instruction::I32ShrU),
            Lang::I32Popcnt(_) => Ok(Instruction::I32Popcnt),
            Lang::I32Const(val) => Ok(Instruction::I32Const(*val)),
            _ => Err(crate::Error::UnsupportedEggLangType),
        }
    }

    fn write2wasm(
        &self,
        info: &ModuleInfo,
        rnd: &mut rand::prelude::SmallRng,
        id_to_node: Vec<&Lang>,
        operands: Vec<Vec<Id>>,
        operators: &Vec<TupleType>,
        newfunc: &mut Function,
        symbolmap: HashMap<&str, Range>,
    ) -> Result<()> {
        enum Event {
            Enter,
            Exit,
        }
        let root = Id::from(0); //id_to_node[0];

        let mut to_visit = vec![(Event::Exit, root), (Event::Enter, root)];
        let random_pool: i32 = rnd.gen();
        // Lets save all of them for now for sake of debugging, but each instruction can be written as soon as they can
        let mut instructions: Vec<Instruction> = Vec::new();
        while let Some((event, node)) = to_visit.pop() {
            match event {
                Event::Enter => {
                    let start_children = to_visit.len();

                    // Check, Not necesarilly, custom operations could do something different
                    let root = id_to_node[usize::from(node)];

                    println!("{:?}", root);

                    if let Lang::Unfold(_) = root {
                        // Avoid to enter here
                        println!("Avoid to enter here")
                    } else {
                        for child in operands[usize::from(node)].iter().copied() {
                            to_visit.push((Event::Enter, child));
                            to_visit.push((Event::Exit, child));
                        }
                    }
                    // Reverse to make it so that we visit children in order
                    // (e.g. operands are visited in order).
                    to_visit[start_children..].reverse();
                }
                Event::Exit => {
                    let operand = id_to_node[usize::from(node)];
                    let instruction = match operand {
                        Lang::Symbol(s1) => {
                            // If a pure symbol was reached, then do an automatic mapping between the wasmparser and wasm-encoder
                            // Replace this by a copy of a chunk of the initial wasm

                            // TODO, implement Unfold 
                            let operators_range = symbolmap[&s1.as_str()].clone();
                            let operators = &operators[operators_range.start..operators_range.end + 1/* take to the next operator to save the offset */];

                            println!("Writing symbol {:?} {:?}", s1, operators);

                            // Copy exactly the same bnytes from the original wasm
                            let raw_range = (
                                operators[0].1, // offset
                                operators[operators.len() - 1].1,
                            );

                            println!("raw range {:?}", raw_range);

                            let raw_data = &info.get_code_section().data[raw_range.0..raw_range.1];
                            println!("raw data {:?}", raw_data);

                            newfunc.raw(raw_data.iter().copied());
                            vec![]
                        } // Map symbol against real value
                        Lang::Rand => vec![Instruction::I32Const(random_pool)], // The same random always ?
                        Lang::Unfold(ops) => {
                            assert_eq!(1, ops.len());

                            // TODO, Replace this with custom functions like previous mutators were
                            println!("Custom mutator unfold");
                            // get operand...expecting a constant
                            let operands = operands[usize::from(node)].clone();


                            let symbol = id_to_node[usize::from(operands[0])];

                            println!("{:?} {:?}", operands, symbol);

                            if let Lang::Symbol(s) = symbol {
                                let operators_range = symbolmap[&s.as_str()].clone();
                                let operators =
                                    &operators[operators_range.start..operators_range.end];
                                assert_eq!(operators.len(), 1);

                                let (i32const, _) = &operators[0];
                                if let Operator::I32Const { value } = i32const {
                                    let r: i32 = rnd.gen();
                                    vec![
                                        Instruction::I32Const(r),
                                        Instruction::I32Const((Wrapping(*value) - Wrapping(r)).0),
                                        Instruction::I32Add,
                                    ]
                                } else {
                                    panic!("{:?}", &operators[0]);
                                    vec![]
                                }
                            } else {
                                panic!("{:?}", symbol);
                                vec![]
                            }
                        }
                        // Add custom mapping above, otherwise it will pass to the default mapping
                        _ => vec![PeepholeMutator::lang2wasm(operand)?],
                    };
                    instructions.extend(instruction);
                }
            }
        }

        println!("{:?}", instructions);

        for &instruction in &instructions {
            newfunc.instruction(instruction);
        }
        Ok(())
    }

    fn generate_random_tree(
        &self,
        info: &ModuleInfo,
        rnd: &mut rand::prelude::SmallRng,
        root: Id,
        costs: &HashMap<Id, (usize, usize)>,
        egraph: &EGraph<Lang, PeepholeMutationAnalysis>,
        operators: &Vec<TupleType>,
        newfunc: &mut Function,
        symbolmap: HashMap<&str, Range>,
    ) -> Result<()> {
        // A map from a node's id to its actual node data.
        let mut id_to_node = vec![];
        // A map from a parent node id to its child operand node ids.
        let mut operands = vec![];

        let root_idx = 0; //rnd.gen_range(0, egraph[root].nodes.len());

        id_to_node.push(&egraph[root].nodes[root_idx]);
        operands.push(vec![]);
        let maxdepth = 0; // FIX to generate large ones

        let rootnode = &egraph[root].nodes[root_idx];

        let mut worklist: Vec<_> = rootnode
            .children()
            .iter()
            .map(|id| (Id::from(root_idx), id, 0)) // (root, operant, depth)
            .collect();

        while let Some((parent, &node, depth)) = worklist.pop() {
            let node_idx = if depth >= maxdepth {
                // look nearest leaf path, in this case, the best in AST size
                costs[&node].1
            } else {
                rnd.gen_range(0, egraph[node].nodes.len())
            };

            let operand = Id::from(id_to_node.len());
            id_to_node.push(&egraph[node].nodes[node_idx]);
            operands.push(vec![]);

            operands[usize::from(parent)].push(operand);

            //let operand = &egraph[node].nodes[node_idx];

            worklist.extend(
                egraph[node].nodes[node_idx]
                    .children()
                    .iter()
                    .map(|id| (operand, id, depth + 1)),
            );
        }

        println!(" random selection {:?} {:?}", id_to_node, operands);

        self.write2wasm(
            info, rnd, id_to_node, operands, operators, newfunc, symbolmap,
        )?;
        Ok(())
    }

    fn random_mutate(
        &self,
        config: &crate::WasmMutate,
        rnd: &mut rand::prelude::SmallRng,
        info: &mut crate::ModuleInfo,
        rules: &[Rewrite<Lang, PeepholeMutationAnalysis>]
    ) -> Result<MutationContext> {
        
        let start = "(i32.shl ?x 1)".parse().unwrap();

        let runner = Runner::default().with_expr(&start).run(rules);
        let egraph = runner.egraph;
        let costs = PeepholeMutator::find_costs(&egraph, NoPopcnt);

        // println!("{:?}", costs);

        let code_section = info.get_code_section();
        let mut sectionreader = CodeSectionReader::new(code_section.data, 0)?;
        let function_count = sectionreader.get_count();

        // Split where to start looking for mutable function
        // In theory random split will provide a mutable location faster
        let function_to_mutate = rnd.gen_range(0, function_count);
        let all_readers = (0..function_count)
            .map(|fidx| sectionreader.read().unwrap())
            .collect::<Vec<FunctionBody>>();
        for fidx in (function_to_mutate..function_count).chain(0..function_to_mutate) {
            let reader = all_readers[fidx as usize];
            let operatorreader = reader.get_operators_reader()?;
            let operatorsrange = operatorreader.reader.range();
            let operators = operatorreader
                .into_iter_with_offsets()
                .collect::<wasmparser::Result<Vec<TupleType>>>()?;
            let operatorscount = operators.len();
            let opcode_to_mutate = rnd.gen_range(0, operatorscount);

            for oidx in (opcode_to_mutate..operatorscount).chain(0..opcode_to_mutate) {
                //let mut applicable = Vec::new();
                //let (operator, offset) = &operators[oidx];
                let eterm = self.operator2term(&operators, oidx);
                if let Some((eterm, symbolmap, start_at)) = eterm {
                    println!("{:?}", eterm);
                    let pattern: Pattern<Lang> = eterm.parse().unwrap();

                    //println!("{:?}", &egraph);
                    let matches = pattern.search(&egraph);
                    // is empty does not work since it looks for any match in the subtrees
                    if !matches.is_empty() {
                        println!("{:?} {:?}", eterm, symbolmap);

                        // Apply random
                        let random_root_idx = 0; //always the first match? hack
                        let random_root = &matches[random_root_idx];

                        let eclass = random_root.eclass;
                        // Check if it is a leaf, hack
                        if egraph[eclass].nodes[random_root_idx].children().len() == 0 {
                            continue;
                        };
                        //println!("{:?}", matches);

                        // Create the new function
                        let mut localreader = reader.get_locals_reader().unwrap();
                        // Get current locals and map to encoder types
                        let mut local_count = 0;
                        let mut current_locals = (0..localreader.get_count())
                            .map(|f| {
                                let (count, ty) = localreader.read().unwrap();
                                local_count += count;
                                (count, map_type(ty).unwrap())
                            })
                            .collect::<Vec<(u32, ValType)>>();

                        // Copy previous function code
                        let (op1, _) = &operators[oidx];
                        println!("{:?} {:?}", op1, start_at);
                        let mut newfunc = Function::new(current_locals /*copy locals here*/);
                        let previous = &code_section.data
                            [operatorsrange.start..operatorsrange.start + start_at];
                        newfunc.raw(previous.iter().copied());

                        self.generate_random_tree(
                            &info,
                            rnd,
                            eclass,
                            &costs,
                            &egraph,
                            &operators,
                            &mut newfunc,
                            symbolmap,
                        )?;
                        // Copy remaining body
                        let (_, offset) = operators[oidx + 1];
                        let ending = &code_section.data[offset..operatorsrange.end];
                        newfunc.raw(ending.iter().copied());
                        return Ok((newfunc, fidx));
                    }
                }
            }
        }

        Err(crate::Error::NotMatchingPeepholes)
    }

    /// To separate the methods will allow us to test rule by rule
    fn mutate_with_rules(&self,
        config: &crate::WasmMutate,
        rnd: &mut rand::prelude::SmallRng,
        info: &mut crate::ModuleInfo,
        rules: &[Rewrite<Lang, PeepholeMutationAnalysis>]
        ) -> Result<Module> {

        let (new_function, function_to_mutate) = self.random_mutate(config, rnd, info, rules)?;

        let mut codes = CodeSection::new();
        let code_section = info.get_code_section();
        let mut sectionreader = CodeSectionReader::new(code_section.data, 0)?;

        for fidx in 0..info.function_count {
            let mut reader = sectionreader.read()?;
            if fidx == function_to_mutate {
                codes.function(&new_function);
            } else {
                codes.raw(&code_section.data[reader.range().start..reader.range().end]);
            }
        }

        let module = info.replace_section(info.code.unwrap(), &codes);
        Ok(module)
    }
}

/// Meta mutator for peephole
impl Mutator for PeepholeMutator {
    fn mutate(
        &self,
        config: &crate::WasmMutate,
        rnd: &mut rand::prelude::SmallRng,
        info: &mut crate::ModuleInfo,
    ) -> Result<Module> {

        let rules: &[Rewrite<Lang, PeepholeMutationAnalysis>] = &[
            rewrite!("unfold-1";  "?x" => "(i32.add rand (i32.sub rand ?x))"),
            rewrite!("unfold-2";  "?x" => "(unfold ?x)"), // Use a custom instruction-mutator for this
            rewrite!("strength-undo";  "(i32.shl ?x 1)" => "(i32.mul ?x ?x)"),
            rewrite!("idempotent-1";  "?x" => "(i32.or ?x ?x)"),
            rewrite!("idempotent-2";  "?x" => "(i32.and ?x ?x)"),
        ];

        self.mutate_with_rules(config, rnd, info, rules)
    }

    fn can_mutate<'a>(
        &self,
        config: &'a crate::WasmMutate,
        info: &crate::ModuleInfo,
    ) -> Result<bool> {
        Ok(info.has_code() && info.function_count > 0)
    }
}

use std::fmt::Debug;
impl Debug for Box<dyn CodeMutator> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Code mutator").finish()
    }
}
pub(crate) trait CodeMutator {
    fn mutate(
        &self,
        config: &WasmMutate,
        rnd: &mut SmallRng,
        operator_index: usize,
        operators: Vec<TupleType>,
        funcreader: FunctionBody,
        body_range: wasmparser::Range,
        function_data: &[u8],
    ) -> Result<Function>;

    /// Returns if this mutator can be applied to the opcode at index i
    fn can_mutate<'a>(
        &self,
        config: &'a WasmMutate,
        operators: &Vec<TupleType<'a>>,
        at: usize,
    ) -> Result<bool>;

    /// Provides the name of the mutator, mostly used for debugging purposes
    fn name(&self) -> String {
        return format!("{:?}", std::any::type_name::<Self>());
    }
}

// This macro is meant to be used for testing deep mutators
// It receives the original wat text variable, the expression returning the mutated function and the expected wat
// For an example, look at SwapCommutativeOperator
#[cfg(test)]
#[macro_export]
macro_rules! match_code_mutation {
    ($wat: ident, $mutation:expr, $expected:ident) => {{
        let original = &wat::parse_str($wat).unwrap();

        let mut parser = Parser::new(0);
        let config = WasmMutate::default();

        let mut offset = 0;

        let mut modu = Module::new();
        let mut codesection = CodeSection::new();

        loop {
            let (payload, chunksize) = match parser.parse(&original[offset..], true).unwrap() {
                Chunk::NeedMoreData(_) => {
                    panic!("This should not be reached");
                }
                Chunk::Parsed { consumed, payload } => (payload, consumed),
            };
            offset += chunksize;

            match payload {
                Payload::TypeSection(reader) => {
                    modu.section(&RawSection {
                        id: SectionId::Type.into(),
                        data: &original[reader.range().start..reader.range().end],
                    });
                }
                Payload::FunctionSection(reader) => {
                    modu.section(&RawSection {
                        id: SectionId::Function.into(),
                        data: &original[reader.range().start..reader.range().end],
                    });
                }
                Payload::ExportSection(reader) => {
                    modu.section(&RawSection {
                        id: SectionId::Export.into(),
                        data: &original[reader.range().start..reader.range().end],
                    });
                }
                Payload::CodeSectionEntry(reader) => {
                    let operatorsreader = reader.get_operators_reader().unwrap();
                    let range = operatorsreader.reader.range();
                    let operators = operatorsreader
                        .into_iter_with_offsets()
                        .collect::<wasmparser::Result<Vec<TupleType>>>()
                        .unwrap();
                    let mutated = $mutation(&config, operators, reader, range, original);
                    codesection.function(&mutated);
                }
                wasmparser::Payload::End => break,
                _ => {
                    // do nothing
                }
            }
        }
        modu.section(&codesection);
        let mutated = modu.finish();
        let mut validator = wasmparser::Validator::new();
        crate::validate(&mut validator, &mutated);

        let text = wasmprinter::print_bytes(mutated).unwrap();

        // parse expected to use the same formatter
        let expected_bytes = &wat::parse_str($expected).unwrap();
        let expectedtext = wasmprinter::print_bytes(expected_bytes).unwrap();
        assert_eq!(text, expectedtext);
    }};
}

#[cfg(test)]
mod tests {
    use crate::{
        mutators::{peephole::PeepholeMutator, Mutator},
        WasmMutate,
    };
    use egg::{Rewrite, rewrite};
    use rand::{rngs::SmallRng, SeedableRng};

    use super::PeepholeMutationAnalysis;

    
    #[test]
    fn test_peep_unfold1(){

        let rules: &[Rewrite<super::Lang, PeepholeMutationAnalysis>] = &[
            rewrite!("unfold-1";  "?x" => "(i32.add rand (i32.sub rand ?x))"),
        ];

        
        test_peephole_mutator(
            r#"
            (module
                (func (export "exported_func") (result i32) (local i32 i32)
                    i32.const 56
                    i32.const 1
                    i32.shl
                )
            )
            "#,
            rules, r#"
            (module
                (type (;0;) (func (result i32)))
                (func (;0;) (type 0) (result i32)
                  (local i32 i32)
                  i32.const 56
                  i32.const 1866069016
                  i32.sub
                  i32.const 1866069016
                  i32.add
                  i32.const 1
                  i32.shl)
                (export "exported_func" (func 0)))
            "#);
    }


    #[test]
    fn test_peep_unfold2(){

        let rules: &[Rewrite<super::Lang, PeepholeMutationAnalysis>] = &[
            rewrite!("unfold-2";  "?x" => "(unfold ?x)"), 
        ];

        test_peephole_mutator(
            r#"
            (module
                (func (export "exported_func") (result i32) (local i32 i32)
                    i32.const 56
                    i32.const 1
                    i32.shl
                )
            )
            "#,
            rules, r#"
            (module
                (type (;0;) (func (result i32)))
                (func (;0;) (type 0) (result i32)
                  (local i32 i32)
                  i32.const 397368787
                  i32.const -397368731
                  i32.add
                  i32.const 1
                  i32.shl)
                (export "exported_func" (func 0)))
            "#);
    }
    


    #[test]
    fn test_peep_strength(){

        let rules: &[Rewrite<super::Lang, PeepholeMutationAnalysis>] = &[
            rewrite!("strength-undo";  "(i32.shl ?x 1)" => "(i32.mul ?x ?x)"),
        ];

        test_peephole_mutator(
        r#"
        (module
            (func (export "exported_func") (result i32) (local i32 i32)
                i32.const 56
                i32.const 1
                i32.shl
            )
        )
        "#,
        rules, r#"
        (module
            (type (;0;) (func (result i32)))
            (func (;0;) (type 0) (result i32)
              (local i32 i32)
              i32.const 56
              i32.const 56
              i32.mul)
            (export "exported_func" (func 0)))"#);
    }
    

    #[test]
    fn test_peep_idem1(){

        let rules: &[Rewrite<super::Lang, PeepholeMutationAnalysis>] = &[
            rewrite!("idempotent-1";  "?x" => "(i32.or ?x ?x)"),
        ];

        test_peephole_mutator(
        r#"
        (module
            (func (export "exported_func") (result i32) (local i32 i32)
                i32.const 56
                i32.const 1
                i32.shl
            )
        )
        "#,
        rules, r#"
        (module
            (type (;0;) (func (result i32)))
            (func (;0;) (type 0) (result i32)
              (local i32 i32)
              i32.const 56
              i32.const 56
              i32.or
              i32.const 1
              i32.shl)
            (export "exported_func" (func 0)))"#);
    }
    
    fn test_peephole_mutator(original: &str, rules: &[Rewrite<super::Lang, PeepholeMutationAnalysis>], expected: &str) {
        
        let wasmmutate = WasmMutate::default();
        let original = &wat::parse_str(original).unwrap();

        let mutator = PeepholeMutator; // the string is empty

        let mut info = wasmmutate.get_module_info(original).unwrap();
        let can_mutate = mutator.can_mutate(&wasmmutate, &info).unwrap();

        let mut rnd = SmallRng::seed_from_u64(0);

        assert_eq!(can_mutate, true);

        let mutated = mutator.mutate_with_rules(&wasmmutate, &mut rnd, &mut info, rules).unwrap();

        let mut validator = wasmparser::Validator::new();
        let mutated_bytes = &mutated.finish();
        crate::validate(&mut validator, mutated_bytes);
        let text = wasmprinter::print_bytes(mutated_bytes).unwrap();

        println!("{}", text);

        let expected_bytes = &wat::parse_str(expected).unwrap();
        let expectedtext = wasmprinter::print_bytes(expected_bytes).unwrap();

        assert_eq!(text, expectedtext);
    }
}
