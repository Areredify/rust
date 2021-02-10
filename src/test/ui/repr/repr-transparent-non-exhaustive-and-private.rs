//aux-build: repr-transparent-non-exhaustive-and-private.rs

#![deny(external_non_exhaustive_members_in_transparent_types)]

extern crate repr_transparent_non_exhaustive_and_private;
use repr_transparent_non_exhaustive_and_private::*;

#[repr(transparent)]
struct Foo1(u32, NonExhZstStruct);
//~^ ERROR zero-sized field in transparent struct can't contain external
//~^^ WARN this was previously accepted

#[repr(transparent)]
struct Foo2(u32, NonExhZstEnum);
//~^ ERROR zero-sized field in transparent struct can't contain external
//~^^ WARN this was previously accepted

#[repr(transparent)]
struct Foo3(u32, (((NonExhVariantEnum, ()), NonExhZstEnum, ()), ()));
//~^ ERROR zero-sized field in transparent struct can't contain external
//~^^ WARN this was previously accepted
//~^^^ ERROR zero-sized field in transparent struct can't contain external
//~^^^^ WARN this was previously accepted

#[repr(transparent)]
struct NonExhPrimaryField(NonExhNonZst, ());

#[repr(transparent)]
struct SeveralZst(NonExhZstStruct, u32, NonExhZstStruct);
//~^ ERROR zero-sized field in transparent struct can't contain external
//~^^ WARN this was previously accepted
//~^^^ ERROR zero-sized field in transparent struct can't contain external
//~^^^^ WARN this was previously accepted

#[repr(transparent)]
struct Foo4(u32, std::marker::PhantomData<NonExhZstStruct>);

struct Bar<T> {
    x: T
}

#[repr(transparent)]
struct IndirectZst(u32, Bar<NonExhZstStruct>);
//~^ ERROR zero-sized field in transparent struct can't contain external
//~^^ WARN this was previously accepted

fn main() {}