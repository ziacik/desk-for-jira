fn main() {
    glib_build_tools::compile_resources(
        &["data/icons"],
        "data/icons/resources.gresource.xml",
        "desk-for-jira.gresource",
    );
}
