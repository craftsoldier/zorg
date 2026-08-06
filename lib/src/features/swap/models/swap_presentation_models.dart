enum SwapStepState { done, active, pending, warning }

class SwapStep {
  const SwapStep({
    required this.label,
    required this.state,
    required this.evidence,
  });

  final String label;
  final SwapStepState state;
  final String evidence;
}

class SwapDetailField {
  const SwapDetailField({required this.label, required this.value});

  final String label;
  final String value;
}
