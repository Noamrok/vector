use lookup_lib::LookupBuf;
use vrl::{prelude::*, value::kind::merge};

#[derive(Clone, Copy, Debug)]
pub struct Unnest;

impl Function for Unnest {
    fn identifier(&self) -> &'static str {
        "unnest"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[Parameter {
            keyword: "path",
            kind: kind::ARRAY,
            required: true,
        }]
    }

    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                title: "external target",
                source: indoc! {r#"
                    . = {"hostname": "localhost", "events": [{"message": "hello"}, {"message": "world"}]}
                    . = unnest(.events)
                "#},
                result: Ok(
                    r#"[{"hostname": "localhost", "events": {"message": "hello"}}, {"hostname": "localhost", "events": {"message": "world"}}]"#,
                ),
            },
            Example {
                title: "variable target",
                source: indoc! {r#"
                    foo = {"hostname": "localhost", "events": [{"message": "hello"}, {"message": "world"}]}
                    foo = unnest(foo.events)
                "#},
                result: Ok(
                    r#"[{"hostname": "localhost", "events": {"message": "hello"}}, {"hostname": "localhost", "events": {"message": "world"}}]"#,
                ),
            },
        ]
    }

    fn compile(
        &self,
        _state: &state::Compiler,
        _ctx: &FunctionCompileContext,
        mut arguments: ArgumentList,
    ) -> Compiled {
        let path = arguments.required_query("path")?;

        Ok(Box::new(UnnestFn { path }))
    }
}

#[derive(Debug, Clone)]
struct UnnestFn {
    path: expression::Query,
}

impl UnnestFn {
    #[cfg(test)]
    fn new(path: &str) -> Self {
        use std::str::FromStr;

        Self {
            path: expression::Query::new(
                expression::Target::External,
                FromStr::from_str(path).unwrap(),
            ),
        }
    }
}

impl Expression for UnnestFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let path = self.path.path();

        let value: Value;
        let target: Box<&dyn Target> = match self.path.target() {
            expression::Target::External => Box::new(ctx.target()) as Box<_>,
            expression::Target::Internal(v) => {
                let v = ctx.state().variable(v.ident()).unwrap_or(&Value::Null);
                Box::new(v as &dyn Target) as Box<_>
            }
            expression::Target::Container(expr) => {
                value = expr.resolve(ctx)?;
                Box::new(&value as &dyn Target) as Box<&dyn Target>
            }
            expression::Target::FunctionCall(expr) => {
                value = expr.resolve(ctx)?;
                Box::new(&value as &dyn Target) as Box<&dyn Target>
            }
        };

        let root = target
            .target_get(&LookupBuf::root())
            .expect("must never fail")
            .expect("always a value");

        let values = root
            .get_by_path(path)
            .cloned()
            .ok_or(value::Error::Expected {
                got: Kind::null(),
                expected: Kind::array(Collection::any()),
            })?
            .try_array()?;

        let events = values
            .into_iter()
            .map(|value| {
                let mut event = root.clone();
                event.insert_by_path(path, value);
                event
            })
            .collect::<Vec<_>>();

        Ok(Value::Array(events))
    }

    fn type_def(&self, state: &state::Compiler) -> TypeDef {
        use expression::Target;

        match self.path.target() {
            Target::External => match state.target_kind().cloned().map(TypeDef::from) {
                Some(root_type_def) => invert_array_at_path(&root_type_def, self.path.path()),
                None => self.path.type_def(state).restrict_array().add_null(),
            },
            Target::Internal(v) => invert_array_at_path(&v.type_def(state), self.path.path()),
            Target::FunctionCall(f) => invert_array_at_path(&f.type_def(state), self.path.path()),
            Target::Container(c) => invert_array_at_path(&c.type_def(state), self.path.path()),
        }
    }
}

/// Assuming path points at an Array, this will take the typedefs for that array,
/// And will remove it returning a set of it's elements.
///
/// For example the typedef for this object:
/// `{ "nonk" => { "shnoog" => [ { "noog" => 2 }, { "noog" => 3 } ] } }`
///
/// Is converted to a typedef for this array:
/// `[ { "nonk" => { "shnoog" => { "noog" => 2 } } },
///    { "nonk" => { "shnoog" => { "noog" => 3 } } },
///  ]`
///
pub fn invert_array_at_path(typedef: &TypeDef, path: &LookupBuf) -> TypeDef {
    use self::value::kind::insert;

    let type_def = typedef.at_path(&path.to_lookup());

    let mut array = Kind::from(type_def)
        .into_array()
        .unwrap_or_else(Collection::any);

    array.known_mut().values_mut().for_each(|kind| {
        let mut tdkind = typedef.kind().clone();
        tdkind
            .insert_at_path(
                &path.to_lookup(),
                kind.clone(),
                insert::Strategy {
                    inner_conflict: insert::InnerConflict::Replace,
                    leaf_conflict: insert::LeafConflict::Replace,
                    coalesced_path: insert::CoalescedPath::InsertAll,
                },
            )
            .expect("infallible");

        *kind = tdkind.clone();
    });

    let mut tdkind = typedef.kind().clone();

    let unknown = array.unknown().map(|unknown| {
        tdkind
            .insert_at_path(
                &path.to_lookup(),
                unknown.clone().into(),
                insert::Strategy {
                    inner_conflict: insert::InnerConflict::Merge(merge::Strategy {
                        depth: merge::Depth::Deep,
                        indices: merge::Indices::Keep,
                    }),
                    leaf_conflict: insert::LeafConflict::Replace,
                    coalesced_path: insert::CoalescedPath::InsertAll,
                },
            )
            .expect("infallible");
        tdkind
    });

    array.set_unknown(unknown);

    TypeDef::array(array)
}

#[cfg(test)]
mod tests {
    use vector_common::{btreemap, TimeZone};

    use super::*;

    #[test]
    fn type_def() {
        struct TestCase {
            old: TypeDef,
            path: &'static str,
            new: TypeDef,
        }

        let cases = vec![
            // Simple case
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { array [
                        type_def! { object {
                            "noog" => type_def! { bytes },
                            "nork" => type_def! { bytes },
                        } },
                    ] },
                } },
                path: ".nonk",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { object {
                            "noog" => type_def! { bytes },
                            "nork" => type_def! { bytes },
                        } },
                    } },
                ] },
            },
            // Provided example
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { object {
                        "shnoog" => type_def! { array [
                            type_def! { object {
                                "noog" => type_def! { bytes },
                            } },
                        ] },
                    } },
                } },
                path: "nonk.shnoog",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { object {
                            "shnoog" => type_def! { object {
                                "noog" => type_def! { bytes },
                            } },
                        } },
                    } },
                ] },
            },
            // Same field in different branches
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { object {
                        "shnoog" => type_def! { array [
                            type_def! { object {
                                "noog" => type_def! { bytes },
                            } },
                        ] },
                    } },
                    "nink" => type_def! { object {
                        "shnoog" => type_def! { array [
                            type_def! { object {
                                "noog" => type_def! { bytes },
                            } },
                        ] },
                    } },
                } },
                path: "nonk.shnoog",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { object {
                            "shnoog" => type_def! { object {
                                "noog" => type_def! { bytes },
                            } },
                        } },
                        "nink" => type_def! { object {
                            "shnoog" => type_def! { array [
                                type_def! { object {
                                    "noog" => type_def! { bytes },
                                } },
                            ] },
                        } },
                    } },
                ] },
            },
            // Indexed any
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { array [
                        type_def! { object {
                            "noog" => type_def! { array [
                                type_def! { bytes },
                            ] },
                            "nork" => type_def! { bytes },
                        } },
                    ] },
                } },
                path: ".nonk[0].noog",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { array {
                            unknown => type_def! { object {
                                "noog" => type_def! { array [
                                    type_def! { bytes },
                                ] },
                                "nork" => type_def! { bytes },
                            } },
                            // The index is added on top of the "unknown" entry.
                            0 => type_def! { object {
                                "noog" => type_def! { bytes },
                            } },
                        } },
                    } },
                ] },
            },
            // Indexed specific
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { array {
                        0 => type_def! { object {
                            "noog" => type_def! { array [
                                type_def! { bytes },
                            ] },
                            "nork" => type_def! { bytes },
                        } },
                    } },
                } },
                path: ".nonk[0].noog",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { array {
                            // The index is added on top of the Any entry.
                            0 => type_def! { object {
                                "noog" => type_def! { bytes },
                                "nork" => type_def! { bytes },
                            } },
                        } },
                    } },
                ] },
            },
            // More nested
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { object {
                        "shnoog" => type_def! { array [
                            type_def! { object {
                                "noog" => type_def! { bytes },
                                "nork" => type_def! { bytes },
                            } },
                        ] },
                    } },
                } },
                path: ".nonk.shnoog",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { object {
                            "shnoog" => type_def! { object {
                                "noog" => type_def! { bytes },
                                "nork" => type_def! { bytes },
                            } },
                        } },
                    } },
                ] },
            },
            //// Coalesce with known path first
            ////
            //// FIXME(Jean): There's still a bug in the `InsertValid` implementation that prevents
            //// this from working as expected.
            ////
            //// I'm assuming it has something to do with us _inserting_ the coalesced field, and
            //// _then_ checking if a certain field in the list of coalesced fields can be null at
            //// runtime. Instead, we should check so _before_ inserting the field.
            ////
            //// This is existing behavior though, and it's not "breaking" anything, in that the
            //// resulting type definition is more expansive than it needs to be, requiring operators
            //// to add more type coercing, but it'd be nice to fix this at some point.
            //TestCase {
            //    old: type_def! { object {
            //        "nonk" => type_def! { object {
            //            "shnoog" => type_def! { array [
            //                type_def! { object {
            //                    "noog" => type_def! { bytes },
            //                    "nork" => type_def! { bytes },
            //                } },
            //            ] },
            //        } },
            //    } },
            //    path: ".(nonk | nork).shnoog",
            //    new: type_def! { array [
            //        type_def! { object {
            //            "nonk" => type_def! { object {
            //                "shnoog" => type_def! { object {
            //                    "noog" => type_def! { bytes },
            //                    "nork" => type_def! { bytes },
            //                } },
            //            } },
            //        } },
            //    ] },
            //},
            // Coalesce with known path second
            TestCase {
                old: type_def! { object {
                    unknown => type_def! { bytes },
                    "nonk" => type_def! { object {
                        "shnoog" => type_def! { array [
                            type_def! { object {
                                "noog" => type_def! { bytes },
                                "nork" => type_def! { bytes },
                            } },
                        ] },
                    } },
                } },
                path: ".(nork | nonk).shnoog",
                new: type_def! { array [
                    type_def! { object {
                        unknown => type_def! { bytes },
                        "nonk" => type_def! { object {
                            "shnoog" => type_def! { object {
                                "noog" => type_def! { bytes },
                                "nork" => type_def! { bytes },
                            } },
                        } },
                        "nork" => type_def! { object {
                            "shnoog" => type_def! { object {
                                "noog" => type_def! { bytes },
                                "nork" => type_def! { bytes },
                            } },
                        } },
                    } },
                ] },
            },
            // Non existent, the types we know are moved into the returned array.
            TestCase {
                old: type_def! { object {
                    "nonk" => type_def! { bytes },
                } },
                path: ".norg",
                new: type_def! { array [
                    type_def! { object {
                        "nonk" => type_def! { bytes },
                        "norg" => type_def! { unknown },
                    } },
                ] },
            },
        ];

        for case in cases {
            let path = LookupBuf::from_str(case.path).unwrap();
            let new = invert_array_at_path(&case.old, &path);

            assert_eq!(case.new, new, "{}", path);
        }
    }

    #[test]
    fn unnest() {
        let cases = vec![
            (
                value!({"hostname": "localhost", "events": [{"message": "hello"}, {"message": "world"}]}),
                Ok(
                    value!([{"hostname": "localhost", "events": {"message": "hello"}}, {"hostname": "localhost", "events": {"message": "world"}}]),
                ),
                UnnestFn::new("events"),
                type_def! { array [
                    type_def! { object {
                        "hostname" => type_def! { bytes },
                        "events" => type_def! { object {
                            "message" => type_def! { bytes },
                        } },
                    } },
                ] },
            ),
            (
                value!({"hostname": "localhost", "events": [{"message": "hello"}, {"message": "world"}]}),
                Err("expected array, got null".to_owned()),
                UnnestFn::new("unknown"),
                type_def! { array [
                    type_def! { object {
                        "hostname" => type_def! { bytes },
                        "unknown" => type_def! { unknown },
                        "events" => type_def! { array [
                            type_def! { object {
                                "message" => type_def! { bytes },
                            } },
                        ] },
                    } },
                ] },
            ),
            (
                value!({"hostname": "localhost", "events": [{"message": "hello"}, {"message": "world"}]}),
                Err("expected array, got string".to_owned()),
                UnnestFn::new("hostname"),
                type_def! { array [
                    type_def! { object {
                        "hostname" => type_def! { unknown },
                        "events" => type_def! { array [
                            type_def! { object {
                                "message" => type_def! { bytes },
                            } },
                        ] },
                    } },
                ] },
            ),
        ];

        let compiler = state::Compiler::new_with_type_def(TypeDef::object(btreemap! {
            "hostname" => Kind::bytes(),
            "events" => Kind::array(Collection::from_unknown(Kind::object(btreemap! {
                Field::from("message") => Kind::bytes(),
            })),
        )}));

        let tz = TimeZone::default();
        for (object, expected, func, expected_typedef) in cases {
            let mut object = object.clone();
            let mut runtime_state = vrl::state::Runtime::default();
            let mut ctx = Context::new(&mut object, &mut runtime_state, &tz);

            let got_typedef = func.type_def(&compiler);

            let got = func
                .resolve(&mut ctx)
                .map_err(|e| format!("{:#}", anyhow::anyhow!(e)));

            assert_eq!(got, expected);
            assert_eq!(got_typedef, expected_typedef);
        }
    }
}
