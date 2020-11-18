#[cfg(feature = "expose_impl")]
pub mod bft;

#[cfg(not(feature = "expose_impl"))]
mod bft;

// let library users have access to our result type
pub use bft::error::Result;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
