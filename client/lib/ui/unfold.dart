import 'package:flutter/foundation.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/widgets.dart';

/// How long a fold takes to open or close.
const unfoldDuration = Duration(milliseconds: 220);

/// Keeps what is above a fold in place while the fold opens or closes in a
/// reversed timeline. A reversed list grows an item upward, pushing everything
/// above it; with [physics] the scroll offset follows the extent the fold adds
/// or removes instead, so the fold opens downward.
class UnfoldAnchor {
  /// How much the folds grew in the layout under way.
  double _grown = 0;
  bool _claimed = true;

  /// For the timeline's list.
  late final ScrollPhysics physics = _UnfoldPhysics(this);

  void _report(double delta) {
    _grown += delta;
    if (!_claimed) return;
    _claimed = false;
    // Unclaimed by the time the frame is drawn: a later change is not the fold's.
    SchedulerBinding.instance.addPostFrameCallback((_) => _take());
  }

  double _take() {
    final grown = _grown;
    _grown = 0;
    _claimed = true;
    return grown;
  }
}

class _UnfoldPhysics extends ScrollPhysics {
  final UnfoldAnchor anchor;
  const _UnfoldPhysics(this.anchor, {super.parent});

  @override
  _UnfoldPhysics applyTo(ScrollPhysics? ancestor) =>
      _UnfoldPhysics(anchor, parent: buildParent(ancestor));

  @override
  double adjustPositionForNewDimensions({
    required ScrollMetrics oldPosition,
    required ScrollMetrics newPosition,
    required bool isScrolling,
    required double velocity,
  }) {
    final grown = anchor._take();
    if (grown == 0) {
      return super.adjustPositionForNewDimensions(
        oldPosition: oldPosition,
        newPosition: newPosition,
        isScrolling: isScrolling,
        velocity: velocity,
      );
    }
    // Reversed, a fold grows away from the viewport's bottom edge. Scrolling
    // by its growth keeps its top in place, less what a shrink-wrapped
    // viewport took up by growing downward itself. (The extents cannot tell:
    // a lazy list only estimates them.) Not after the parent physics: folding
    // near the top, they would already have pulled the old offset into the
    // smaller range, and the page would move by that much twice.
    final taken = newPosition.viewportDimension - oldPosition.viewportDimension;
    return clampDouble(
      newPosition.pixels + grown - taken,
      newPosition.minScrollExtent,
      newPosition.maxScrollExtent,
    );
  }
}

/// Hands the folds of a timeline its [UnfoldAnchor].
class UnfoldScope extends InheritedWidget {
  final UnfoldAnchor anchor;
  const UnfoldScope({super.key, required this.anchor, required super.child});

  static UnfoldAnchor? of(BuildContext context) =>
      context.getInheritedWidgetOfExactType<UnfoldScope>()?.anchor;

  @override
  bool updateShouldNotify(UnfoldScope oldWidget) => oldWidget.anchor != anchor;
}

/// Reveals what [builder] builds top-down as [open] turns on, and folds it
/// back up as it turns off. With [anchored], what is above stays in place in
/// an [UnfoldScope]'s timeline; leave it off when the fold follows a run
/// rather than the user, so a timeline kept at its newest stays there.
class Unfold extends StatefulWidget {
  final bool open;
  final bool anchored;
  final WidgetBuilder builder;
  const Unfold({
    super.key,
    required this.open,
    required this.builder,
    this.anchored = true,
  });

  @override
  State<Unfold> createState() => _UnfoldState();
}

class _UnfoldState extends State<Unfold> with SingleTickerProviderStateMixin {
  late final controller = AnimationController(
    vsync: this,
    duration: unfoldDuration,
    value: widget.open ? 1 : 0,
  );
  late final curve = CurvedAnimation(
    parent: controller,
    curve: Curves.easeOutCubic,
    reverseCurve: Curves.easeInCubic,
  );

  /// The latest turn was the user's.
  bool anchored = false;

  @override
  void didUpdateWidget(Unfold oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (widget.open == oldWidget.open) return;
    anchored = widget.anchored;
    widget.open ? controller.forward() : controller.reverse();
  }

  @override
  void dispose() {
    curve.dispose();
    controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final anchor = UnfoldScope.of(context);
    return AnimatedBuilder(
      animation: curve,
      builder: (context, child) => _Growth(
        anchor: anchored ? anchor : null,
        factor: curve.value,
        child: controller.isDismissed
            ? const SizedBox(width: double.infinity)
            : ClipRect(
                child: Align(
                  alignment: Alignment.topCenter,
                  heightFactor: curve.value,
                  child: child,
                ),
              ),
      ),
      child: controller.isDismissed ? null : widget.builder(context),
    );
  }
}

class _Growth extends SingleChildRenderObjectWidget {
  final UnfoldAnchor? anchor;
  final double factor;
  const _Growth({required this.anchor, required this.factor, super.child});

  @override
  _RenderGrowth createRenderObject(BuildContext context) =>
      _RenderGrowth(anchor, factor);

  @override
  void updateRenderObject(BuildContext context, _RenderGrowth renderObject) =>
      renderObject
        ..anchor = anchor
        ..factor = factor;
}

/// Tells its anchor when the fold's own animation changed its height; content
/// growing inside an open fold is not the fold's doing.
class _RenderGrowth extends RenderProxyBox {
  _RenderGrowth(this.anchor, this.factor);
  UnfoldAnchor? anchor;
  double factor;
  double? _laidOut;

  @override
  void performLayout() {
    final before = hasSize ? size.height : null;
    super.performLayout();
    if (factor != _laidOut && before != null && before != size.height) {
      anchor?._report(size.height - before);
    }
    _laidOut = factor;
  }
}
