pub(crate) fn normalize_optional_args(args: Option<&str>) -> Option<&str> {
    args.map(str::trim).filter(|args| !args.is_empty())
}
