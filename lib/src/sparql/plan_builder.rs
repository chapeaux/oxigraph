use crate::model::vocab::xsd;
use crate::model::Literal;
use crate::sparql::algebra::*;
use crate::sparql::model::*;
use crate::sparql::plan::*;
use crate::store::numeric_encoder::ENCODED_DEFAULT_GRAPH;
use crate::store::StoreConnection;
use crate::Result;
use failure::format_err;
use std::collections::HashSet;

pub struct PlanBuilder<'a, S: StoreConnection> {
    store: &'a S,
}

impl<'a, S: StoreConnection> PlanBuilder<'a, S> {
    pub fn build(store: &'a S, pattern: &GraphPattern) -> Result<(PlanNode, Vec<Variable>)> {
        let mut variables = Vec::default();
        let plan = PlanBuilder { store }.build_for_graph_pattern(
            pattern,
            PlanNode::Init,
            &mut variables,
            PatternValue::Constant(ENCODED_DEFAULT_GRAPH),
        )?;
        Ok((plan, variables))
    }

    pub fn build_graph_template(
        store: &S,
        template: &[TriplePattern],
        mut variables: Vec<Variable>,
    ) -> Result<Vec<TripleTemplate>> {
        PlanBuilder { store }.build_for_graph_template(template, &mut variables)
    }

    fn build_for_graph_pattern(
        &self,
        pattern: &GraphPattern,
        input: PlanNode,
        variables: &mut Vec<Variable>,
        graph_name: PatternValue,
    ) -> Result<PlanNode> {
        Ok(match pattern {
            GraphPattern::BGP(p) => self.build_for_bgp(p, input, variables, graph_name)?,
            GraphPattern::Join(a, b) => PlanNode::Join {
                left: Box::new(self.build_for_graph_pattern(
                    a,
                    input.clone(),
                    variables,
                    graph_name,
                )?),
                right: Box::new(self.build_for_graph_pattern(b, input, variables, graph_name)?),
            },
            GraphPattern::LeftJoin(a, b, e) => {
                let left = self.build_for_graph_pattern(a, input, variables, graph_name)?;
                let right =
                    self.build_for_graph_pattern(b, PlanNode::Init, variables, graph_name)?;
                //We add the extra filter if needed
                let right = if *e == Expression::from(Literal::from(true)) {
                    right
                } else {
                    PlanNode::Filter {
                        child: Box::new(right),
                        expression: self.build_for_expression(e, variables, graph_name)?,
                    }
                };
                let possible_problem_vars = right
                    .variables()
                    .difference(&left.variables())
                    .cloned()
                    .collect();

                PlanNode::LeftJoin {
                    left: Box::new(left),
                    right: Box::new(right),
                    possible_problem_vars,
                }
            }
            GraphPattern::Filter(e, p) => PlanNode::Filter {
                child: Box::new(self.build_for_graph_pattern(p, input, variables, graph_name)?),
                expression: self.build_for_expression(e, variables, graph_name)?,
            },
            GraphPattern::Union(a, b) => {
                //We flatten the UNIONs
                let mut stack: Vec<&GraphPattern> = vec![a, b];
                let mut children = vec![];
                loop {
                    match stack.pop() {
                        None => break,
                        Some(GraphPattern::Union(a, b)) => {
                            stack.push(a);
                            stack.push(b);
                        }
                        Some(p) => children.push(self.build_for_graph_pattern(
                            p,
                            PlanNode::Init,
                            variables,
                            graph_name,
                        )?),
                    }
                }
                PlanNode::Union {
                    entry: Box::new(input),
                    children,
                }
            }
            GraphPattern::Graph(g, p) => {
                let graph_name = self.pattern_value_from_named_node_or_variable(g, variables)?;
                self.build_for_graph_pattern(p, input, variables, graph_name)?
            }
            GraphPattern::Extend(p, v, e) => PlanNode::Extend {
                child: Box::new(self.build_for_graph_pattern(p, input, variables, graph_name)?),
                position: variable_key(variables, &v),
                expression: self.build_for_expression(e, variables, graph_name)?,
            },
            GraphPattern::Minus(a, b) => PlanNode::AntiJoin {
                left: Box::new(self.build_for_graph_pattern(
                    a,
                    input.clone(),
                    variables,
                    graph_name,
                )?),
                right: Box::new(self.build_for_graph_pattern(b, input, variables, graph_name)?),
            },
            GraphPattern::Service(_n, _p, _s) => unimplemented!(),
            GraphPattern::AggregateJoin(_g, _a) => unimplemented!(),
            GraphPattern::Data(bs) => PlanNode::StaticBindings {
                tuples: self.encode_bindings(bs, variables)?,
            },
            GraphPattern::OrderBy(l, o) => {
                let by: Result<Vec<_>> = o
                    .iter()
                    .map(|comp| match comp {
                        OrderComparator::Asc(e) => Ok(Comparator::Asc(
                            self.build_for_expression(e, variables, graph_name)?,
                        )),
                        OrderComparator::Desc(e) => Ok(Comparator::Desc(
                            self.build_for_expression(e, variables, graph_name)?,
                        )),
                    })
                    .collect();
                PlanNode::Sort {
                    child: Box::new(self.build_for_graph_pattern(l, input, variables, graph_name)?),
                    by: by?,
                }
            }
            GraphPattern::Project(l, new_variables) => PlanNode::Project {
                child: Box::new(self.build_for_graph_pattern(
                    l,
                    input,
                    &mut new_variables.clone(),
                    graph_name,
                )?),
                mapping: new_variables
                    .iter()
                    .map(|variable| variable_key(variables, variable))
                    .collect(),
            },
            GraphPattern::Distinct(l) => PlanNode::HashDeduplicate {
                child: Box::new(self.build_for_graph_pattern(l, input, variables, graph_name)?),
            },
            GraphPattern::Reduced(l) => {
                self.build_for_graph_pattern(l, input, variables, graph_name)?
            }
            GraphPattern::Slice(l, start, length) => {
                let mut plan = self.build_for_graph_pattern(l, input, variables, graph_name)?;
                if *start > 0 {
                    plan = PlanNode::Skip {
                        child: Box::new(plan),
                        count: *start,
                    };
                }
                if let Some(length) = length {
                    plan = PlanNode::Limit {
                        child: Box::new(plan),
                        count: *length,
                    };
                }
                plan
            }
        })
    }

    fn build_for_bgp(
        &self,
        p: &[TripleOrPathPattern],
        input: PlanNode,
        variables: &mut Vec<Variable>,
        graph_name: PatternValue,
    ) -> Result<PlanNode> {
        let mut plan = input;
        for pattern in sort_bgp(p) {
            plan = match pattern {
                TripleOrPathPattern::Triple(pattern) => PlanNode::QuadPatternJoin {
                    child: Box::new(plan),
                    subject: self
                        .pattern_value_from_term_or_variable(&pattern.subject, variables)?,
                    predicate: self
                        .pattern_value_from_named_node_or_variable(&pattern.predicate, variables)?,
                    object: self.pattern_value_from_term_or_variable(&pattern.object, variables)?,
                    graph_name,
                },
                TripleOrPathPattern::Path(_pattern) => unimplemented!(),
            }
        }
        Ok(plan)
    }

    fn build_for_expression(
        &self,
        expression: &Expression,
        variables: &mut Vec<Variable>,
        graph_name: PatternValue,
    ) -> Result<PlanExpression> {
        Ok(match expression {
            Expression::Constant(t) => match t {
                TermOrVariable::Term(t) => {
                    PlanExpression::Constant(self.store.encoder().encode_term(t)?)
                }
                TermOrVariable::Variable(v) => PlanExpression::Variable(variable_key(variables, v)),
            },
            Expression::Or(a, b) => PlanExpression::Or(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::And(a, b) => PlanExpression::And(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::Equal(a, b) => PlanExpression::Equal(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::NotEqual(a, b) => PlanExpression::NotEqual(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::Greater(a, b) => PlanExpression::Greater(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::GreaterOrEq(a, b) => PlanExpression::GreaterOrEq(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::Lower(a, b) => PlanExpression::Lower(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::LowerOrEq(a, b) => PlanExpression::LowerOrEq(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::In(e, l) => PlanExpression::In(
                Box::new(self.build_for_expression(e, variables, graph_name)?),
                self.expression_list(l, variables, graph_name)?,
            ),
            Expression::NotIn(e, l) => PlanExpression::UnaryNot(Box::new(PlanExpression::In(
                Box::new(self.build_for_expression(e, variables, graph_name)?),
                self.expression_list(l, variables, graph_name)?,
            ))),
            Expression::Add(a, b) => PlanExpression::Add(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::Sub(a, b) => PlanExpression::Sub(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::Mul(a, b) => PlanExpression::Mul(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::Div(a, b) => PlanExpression::Div(
                Box::new(self.build_for_expression(a, variables, graph_name)?),
                Box::new(self.build_for_expression(b, variables, graph_name)?),
            ),
            Expression::UnaryPlus(e) => PlanExpression::UnaryPlus(Box::new(
                self.build_for_expression(e, variables, graph_name)?,
            )),
            Expression::UnaryMinus(e) => PlanExpression::UnaryMinus(Box::new(
                self.build_for_expression(e, variables, graph_name)?,
            )),
            Expression::UnaryNot(e) => PlanExpression::UnaryNot(Box::new(
                self.build_for_expression(e, variables, graph_name)?,
            )),
            Expression::FunctionCall(function, parameters) => match function {
                Function::Str => PlanExpression::Str(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Lang => PlanExpression::Lang(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::LangMatches => PlanExpression::LangMatches(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::Datatype => PlanExpression::Datatype(Box::new(
                    self.build_for_expression(&parameters[0], variables, graph_name)?,
                )),
                Function::IRI => PlanExpression::IRI(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::BNode => PlanExpression::BNode(match parameters.get(0) {
                    Some(e) => Some(Box::new(
                        self.build_for_expression(e, variables, graph_name)?,
                    )),
                    None => None,
                }),
                Function::Rand => PlanExpression::Rand,
                Function::Abs => PlanExpression::Abs(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Ceil => PlanExpression::Ceil(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Floor => PlanExpression::Floor(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Round => PlanExpression::Round(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Concat => PlanExpression::Concat(self.expression_list(
                    &parameters,
                    variables,
                    graph_name,
                )?),
                Function::SubStr => PlanExpression::SubStr(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                    match parameters.get(2) {
                        Some(flags) => Some(Box::new(
                            self.build_for_expression(flags, variables, graph_name)?,
                        )),
                        None => None,
                    },
                ),
                Function::StrLen => PlanExpression::StrLen(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Replace => PlanExpression::Replace(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[2], variables, graph_name)?),
                    match parameters.get(3) {
                        Some(flags) => Some(Box::new(
                            self.build_for_expression(flags, variables, graph_name)?,
                        )),
                        None => None,
                    },
                ),
                Function::UCase => PlanExpression::UCase(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::LCase => PlanExpression::LCase(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::EncodeForURI => PlanExpression::EncodeForURI(Box::new(
                    self.build_for_expression(&parameters[0], variables, graph_name)?,
                )),
                Function::Contains => PlanExpression::Contains(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::StrStarts => PlanExpression::StrStarts(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::StrEnds => PlanExpression::StrEnds(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::StrBefore => PlanExpression::StrBefore(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::StrAfter => PlanExpression::StrAfter(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::Year => PlanExpression::Year(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Month => PlanExpression::Month(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Day => PlanExpression::Day(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Hours => PlanExpression::Hours(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Minutes => PlanExpression::Minutes(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Seconds => PlanExpression::Seconds(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Timezone => PlanExpression::Timezone(Box::new(
                    self.build_for_expression(&parameters[0], variables, graph_name)?,
                )),
                Function::Tz => PlanExpression::Tz(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Now => PlanExpression::Now,
                Function::UUID => PlanExpression::UUID,
                Function::StrUUID => PlanExpression::StrUUID,
                Function::MD5 => PlanExpression::MD5(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::SHA1 => PlanExpression::SHA1(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::SHA256 => PlanExpression::SHA256(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::SHA384 => PlanExpression::SHA384(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::SHA512 => PlanExpression::SHA512(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::Coalesce => PlanExpression::Coalesce(self.expression_list(
                    &parameters,
                    variables,
                    graph_name,
                )?),
                Function::If => PlanExpression::If(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[2], variables, graph_name)?),
                ),
                Function::StrLang => PlanExpression::StrLang(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::StrDT => PlanExpression::StrDT(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::SameTerm => PlanExpression::SameTerm(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                ),
                Function::IsIRI => PlanExpression::IsIRI(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::IsBlank => PlanExpression::IsBlank(Box::new(self.build_for_expression(
                    &parameters[0],
                    variables,
                    graph_name,
                )?)),
                Function::IsLiteral => PlanExpression::IsLiteral(Box::new(
                    self.build_for_expression(&parameters[0], variables, graph_name)?,
                )),
                Function::IsNumeric => PlanExpression::IsNumeric(Box::new(
                    self.build_for_expression(&parameters[0], variables, graph_name)?,
                )),
                Function::Regex => PlanExpression::Regex(
                    Box::new(self.build_for_expression(&parameters[0], variables, graph_name)?),
                    Box::new(self.build_for_expression(&parameters[1], variables, graph_name)?),
                    match parameters.get(2) {
                        Some(flags) => Some(Box::new(
                            self.build_for_expression(flags, variables, graph_name)?,
                        )),
                        None => None,
                    },
                ),
                Function::Custom(name) => {
                    if *name == *xsd::BOOLEAN {
                        self.build_cast(
                            parameters,
                            PlanExpression::BooleanCast,
                            variables,
                            graph_name,
                            "boolean",
                        )?
                    } else if *name == *xsd::DOUBLE {
                        self.build_cast(
                            parameters,
                            PlanExpression::DoubleCast,
                            variables,
                            graph_name,
                            "double",
                        )?
                    } else if *name == *xsd::FLOAT {
                        self.build_cast(
                            parameters,
                            PlanExpression::FloatCast,
                            variables,
                            graph_name,
                            "float",
                        )?
                    } else if *name == *xsd::DECIMAL {
                        self.build_cast(
                            parameters,
                            PlanExpression::DecimalCast,
                            variables,
                            graph_name,
                            "decimal",
                        )?
                    } else if *name == *xsd::INTEGER {
                        self.build_cast(
                            parameters,
                            PlanExpression::IntegerCast,
                            variables,
                            graph_name,
                            "integer",
                        )?
                    } else if *name == *xsd::DATE {
                        self.build_cast(
                            parameters,
                            PlanExpression::DateCast,
                            variables,
                            graph_name,
                            "date",
                        )?
                    } else if *name == *xsd::TIME {
                        self.build_cast(
                            parameters,
                            PlanExpression::TimeCast,
                            variables,
                            graph_name,
                            "time",
                        )?
                    } else if *name == *xsd::DATE_TIME {
                        self.build_cast(
                            parameters,
                            PlanExpression::DateTimeCast,
                            variables,
                            graph_name,
                            "dateTime",
                        )?
                    } else if *name == *xsd::STRING {
                        self.build_cast(
                            parameters,
                            PlanExpression::StringCast,
                            variables,
                            graph_name,
                            "string",
                        )?
                    } else {
                        Err(format_err!("Not supported custom function {}", expression))?
                    }
                }
            },
            Expression::Bound(v) => PlanExpression::Bound(variable_key(variables, v)),
            Expression::Exists(n) => PlanExpression::Exists(Box::new(
                self.build_for_graph_pattern(n, PlanNode::Init, variables, graph_name)?,
            )),
        })
    }

    fn build_cast(
        &self,
        parameters: &[Expression],
        constructor: impl Fn(Box<PlanExpression>) -> PlanExpression,
        variables: &mut Vec<Variable>,
        graph_name: PatternValue,
        name: &'static str,
    ) -> Result<PlanExpression> {
        if parameters.len() == 1 {
            Ok(constructor(Box::new(self.build_for_expression(
                &parameters[0],
                variables,
                graph_name,
            )?)))
        } else {
            Err(format_err!(
                "The xsd:{} casting takes only one parameter",
                name
            ))
        }
    }

    fn expression_list(
        &self,
        l: &[Expression],
        variables: &mut Vec<Variable>,
        graph_name: PatternValue,
    ) -> Result<Vec<PlanExpression>> {
        l.iter()
            .map(|e| self.build_for_expression(e, variables, graph_name))
            .collect()
    }

    fn pattern_value_from_term_or_variable(
        &self,
        term_or_variable: &TermOrVariable,
        variables: &mut Vec<Variable>,
    ) -> Result<PatternValue> {
        Ok(match term_or_variable {
            TermOrVariable::Term(term) => {
                PatternValue::Constant(self.store.encoder().encode_term(term)?)
            }
            TermOrVariable::Variable(variable) => {
                PatternValue::Variable(variable_key(variables, variable))
            }
        })
    }

    fn pattern_value_from_named_node_or_variable(
        &self,
        named_node_or_variable: &NamedNodeOrVariable,
        variables: &mut Vec<Variable>,
    ) -> Result<PatternValue> {
        Ok(match named_node_or_variable {
            NamedNodeOrVariable::NamedNode(named_node) => {
                PatternValue::Constant(self.store.encoder().encode_named_node(named_node)?)
            }
            NamedNodeOrVariable::Variable(variable) => {
                PatternValue::Variable(variable_key(variables, variable))
            }
        })
    }

    fn encode_bindings(
        &self,
        bindings: &StaticBindings,
        variables: &mut Vec<Variable>,
    ) -> Result<Vec<EncodedTuple>> {
        let encoder = self.store.encoder();
        let bindings_variables = bindings.variables();
        bindings
            .values_iter()
            .map(move |values| {
                let mut result = vec![None; variables.len()];
                for (key, value) in values.iter().enumerate() {
                    if let Some(term) = value {
                        result[variable_key(variables, &bindings_variables[key])] =
                            Some(encoder.encode_term(term)?);
                    }
                }
                Ok(result)
            })
            .collect()
    }

    fn build_for_graph_template(
        &self,
        template: &[TriplePattern],
        variables: &mut Vec<Variable>,
    ) -> Result<Vec<TripleTemplate>> {
        let mut bnodes = Vec::default();
        template
            .iter()
            .map(|triple| {
                Ok(TripleTemplate {
                    subject: self.template_value_from_term_or_variable(
                        &triple.subject,
                        variables,
                        &mut bnodes,
                    )?,
                    predicate: self.template_value_from_named_node_or_variable(
                        &triple.predicate,
                        variables,
                        &mut bnodes,
                    )?,
                    object: self.template_value_from_term_or_variable(
                        &triple.object,
                        variables,
                        &mut bnodes,
                    )?,
                })
            })
            .collect()
    }

    fn template_value_from_term_or_variable(
        &self,
        term_or_variable: &TermOrVariable,
        variables: &mut Vec<Variable>,
        bnodes: &mut Vec<Variable>,
    ) -> Result<TripleTemplateValue> {
        Ok(match term_or_variable {
            TermOrVariable::Term(term) => {
                TripleTemplateValue::Constant(self.store.encoder().encode_term(term)?)
            }
            TermOrVariable::Variable(variable) => {
                if variable.has_name() {
                    TripleTemplateValue::Variable(variable_key(variables, variable))
                } else {
                    TripleTemplateValue::BlankNode(variable_key(bnodes, variable))
                }
            }
        })
    }

    fn template_value_from_named_node_or_variable(
        &self,
        named_node_or_variable: &NamedNodeOrVariable,
        variables: &mut Vec<Variable>,
        bnodes: &mut Vec<Variable>,
    ) -> Result<TripleTemplateValue> {
        Ok(match named_node_or_variable {
            NamedNodeOrVariable::NamedNode(term) => {
                TripleTemplateValue::Constant(self.store.encoder().encode_named_node(term)?)
            }
            NamedNodeOrVariable::Variable(variable) => {
                if variable.has_name() {
                    TripleTemplateValue::Variable(variable_key(variables, variable))
                } else {
                    TripleTemplateValue::BlankNode(variable_key(bnodes, variable))
                }
            }
        })
    }
}

fn variable_key(variables: &mut Vec<Variable>, variable: &Variable) -> usize {
    match slice_key(variables, variable) {
        Some(key) => key,
        None => {
            variables.push(variable.clone());
            variables.len() - 1
        }
    }
}

fn slice_key<T: Eq>(slice: &[T], element: &T) -> Option<usize> {
    for (i, item) in slice.iter().enumerate() {
        if item == element {
            return Some(i);
        }
    }
    None
}

fn sort_bgp(p: &[TripleOrPathPattern]) -> Vec<&TripleOrPathPattern> {
    let mut assigned_variables = HashSet::default();
    let mut new_p: Vec<_> = p.iter().collect();

    for i in 0..new_p.len() {
        (&mut new_p[i..]).sort_by(|p1, p2| {
            count_pattern_binds(p2, &assigned_variables)
                .cmp(&count_pattern_binds(p1, &assigned_variables))
        });
        add_pattern_variables(new_p[i], &mut assigned_variables);
    }

    new_p
}

fn count_pattern_binds(
    pattern: &TripleOrPathPattern,
    assigned_variables: &HashSet<&Variable>,
) -> u8 {
    let mut count = 3;
    if let TermOrVariable::Variable(v) = pattern.subject() {
        if !assigned_variables.contains(v) {
            count -= 1;
        }
    }
    if let TripleOrPathPattern::Triple(t) = pattern {
        if let NamedNodeOrVariable::Variable(v) = &t.predicate {
            if !assigned_variables.contains(v) {
                count -= 1;
            }
        }
    } else {
        count -= 1;
    }
    if let TermOrVariable::Variable(v) = pattern.object() {
        if !assigned_variables.contains(v) {
            count -= 1;
        }
    }
    count
}

fn add_pattern_variables<'a>(
    pattern: &'a TripleOrPathPattern,
    variables: &mut HashSet<&'a Variable>,
) {
    if let TermOrVariable::Variable(v) = pattern.subject() {
        variables.insert(v);
    }
    if let TripleOrPathPattern::Triple(t) = pattern {
        if let NamedNodeOrVariable::Variable(v) = &t.predicate {
            variables.insert(v);
        }
    }
    if let TermOrVariable::Variable(v) = pattern.object() {
        variables.insert(v);
    }
}