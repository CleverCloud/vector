package metadata

remap: functions: parse_regex: {
	category:    "Parse"
	description: """
		Parses the `value` via the provided [Regex](\(urls.regex)) `pattern`.

		This function differs from the `parse_regex_all` function in that it returns only the first match.
		"""
	notices: [
		"""
		VRL aims to provide purpose-specific [parsing functions](\(urls.vrl_parsing_functions)) for common log formats.
		Before reaching for the `parse_regex` function, see if a VRL [`parse_*` function](\(urls.vrl_parsing_functions))
		already exists for your format. If not, we recommend [opening an issue](\(urls.new_feature_request)) to request
		support for the desired format.
		""",
		"""
			All values are returned as strings. We recommend manually coercing values to desired types as you see fit.
			""",
	]

	arguments: [
		{
			name:        "value"
			description: "The string to search."
			required:    true
			type: ["string"]
		},
		{
			name:        "pattern"
			description: "The regular expression pattern to search against."
			required:    true
			type: ["regex"]
		},
	]
	internal_failure_reasons: [
		"`value` fails to parse using the provided `pattern`",
	]
	return: {
		types: ["object"]
		rules: [
			"Matches return all capture groups corresponding to the leftmost matches in the text.",
			"Raises an error if no match is found.",
		]
	}

	examples: [
		{
			title: "Parse using Regex (with capture groups)"
			source: """
				parse_regex!("first group and second group.", r'(?P<number>.*?) group')
				"""
			return: {
				number: "first"
				"0":    "first group"
				"1":    "first"
			}
		},
		{
			title: "Parse using Regex (without capture groups)"
			source: """
				parse_regex!("first group and second group.", r'(\\w+) group')
				"""
			return: {
				"0": "first group"
				"1": "first"
			}
		},
	]
}
