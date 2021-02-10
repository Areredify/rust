#[non_exhaustive]
pub struct NonExhZstStruct {}

#[non_exhaustive]
pub struct NonExhNonZst { 
    pub x: u32, 
}

#[non_exhaustive]
pub enum NonExhZstEnum {} 

pub enum NonExhVariantEnum {
    #[non_exhaustive]
    A {}
}
