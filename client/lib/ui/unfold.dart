import 'dart:math';

import 'package:flutter/foundation.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/widgets.dart';

/// How long a fold takes to open or close.
const unfoldDuration = Duration(milliseconds: 220);

/// Keeps what is above a fold in place while the fold opens or closes in a
/// reversed timeline. A reversed list grows an item upward, pushing everything
/// above it; with [physics] the scroll offset follows the extent the fold adds
/// or removes instead, so the fold opens downward. Scrolled back from the
/// newest row, [TimelineRows] keeps the page in place itself.
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

/// The rows of a reversed timeline, newest first. While the newest row ends
/// on screen, what grows or arrives there pushes the older rows up, so the
/// newest stays in view. Scrolled back from there, the topmost row on screen
/// keeps its place instead, whatever grows, arrives or folds below it.
class TimelineRows extends SliverList {
  /// The last child loads older rows rather than being one. They arrive below
  /// it, so it never keeps its place.
  final bool loader;
  const TimelineRows({super.key, required super.delegate, this.loader = false});

  @override
  RenderSliverList createRenderObject(BuildContext context) =>
      _RenderTimelineRows(
        childManager: context as SliverMultiBoxAdaptorElement,
        unfold: UnfoldScope.of(context),
        loader: loader,
      );

  @override
  void updateRenderObject(
    BuildContext context,
    RenderSliverList renderObject,
  ) => (renderObject as _RenderTimelineRows)
    ..unfold = UnfoldScope.of(context)
    ..loader = loader;
}

class _RenderTimelineRows extends RenderSliverList {
  _RenderTimelineRows({
    required super.childManager,
    required this.unfold,
    required this.loader,
  });
  UnfoldAnchor? unfold;
  bool loader;

  /// The topmost row on screen as last laid out, and the scroll offset of its
  /// top then.
  RenderBox? _held;
  double _top = 0;

  /// Where the held row's top went if the list dropped it in this layout.
  double? _dropped;

  double _topOf(RenderBox child) =>
      childScrollOffset(child)! + paintExtentOf(child);

  @override
  void performLayout() {
    _dropped = null;
    super.performLayout();
    final reading = constraints.scrollOffset > precisionErrorTolerance;
    // The held row already moves the page by what a fold below it adds;
    // the fold's own anchor would add it again.
    if (reading) unfold?._take();
    final listCorrection = geometry!.scrollOffsetCorrection;
    if (listCorrection != null) {
      // The list renumbered its offsets and moves the page by as much.
      _top += listCorrection;
      return;
    }
    final held = _held;
    final top = held == null
        ? null
        : _laidOut(held)
        ? _topOf(held)
        : _dropped;
    if (reading && top != null) {
      final moved = top - _top;
      if (moved.abs() > precisionErrorTolerance) {
        _top += moved;
        // Not past the newest row's end.
        final back =
            constraints.precedingScrollExtent + constraints.scrollOffset;
        geometry = SliverGeometry(scrollOffsetCorrection: max(moved, -back));
        return;
      }
    }
    final end = constraints.scrollOffset + constraints.remainingPaintExtent;
    _held = null;
    for (var child = firstChild; child != null; child = childAfter(child)) {
      if (childScrollOffset(child)! >= end ||
          loader && indexOf(child) == childManager.childCount - 1) {
        break;
      }
      _held = child;
    }
    if (_held case final held?) _top = _topOf(held);
  }

  /// What grew below the held row can push it out of the rows the list keeps
  /// laid out; the list drops it then, and the page has to follow it all the
  /// same.
  @override
  void collectGarbage(int leadingGarbage, int trailingGarbage) {
    // The rows dropped past the end were not laid out again: the held one
    // follows the last that was, by the extents they had.
    final kept = childCount - trailingGarbage;
    var i = 0;
    double? end;
    for (var child = firstChild; child != null; child = childAfter(child)) {
      final offset = i < kept ? childScrollOffset(child) : end;
      end = offset == null || !child.hasSize
          ? null
          : offset + paintExtentOf(child);
      if (child == _held) {
        if (i < leadingGarbage || i >= kept) _dropped = end;
        break;
      }
      i++;
    }
    super.collectGarbage(leadingGarbage, trailingGarbage);
  }

  bool _laidOut(RenderBox box) {
    for (var child = firstChild; child != null; child = childAfter(child)) {
      if (child == box) return true;
    }
    return false;
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

  /// How much of the top shows while folded.
  final double peek;
  final WidgetBuilder builder;
  const Unfold({
    super.key,
    required this.open,
    required this.builder,
    this.anchored = true,
    this.peek = 0,
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
    bool hidden() => controller.isDismissed && widget.peek == 0;
    return AnimatedBuilder(
      animation: curve,
      builder: (context, child) => _Growth(
        anchor: anchored ? anchor : null,
        factor: curve.value,
        child: hidden()
            ? const SizedBox(width: double.infinity)
            : _Reveal(peek: widget.peek, factor: curve.value, child: child),
      ),
      child: hidden() ? null : widget.builder(context),
    );
  }
}

/// Shows the top of its child: [peek] of it, and [factor] of the rest.
class _Reveal extends SingleChildRenderObjectWidget {
  final double peek, factor;
  const _Reveal({required this.peek, required this.factor, super.child});

  @override
  _RenderReveal createRenderObject(BuildContext context) =>
      _RenderReveal(peek, factor);

  @override
  void updateRenderObject(BuildContext context, _RenderReveal renderObject) =>
      renderObject
        ..peek = peek
        ..factor = factor;
}

/// Sized and placed as a top-centre [Align] with a height factor would be,
/// clipped to what shows.
class _RenderReveal extends RenderShiftedBox {
  _RenderReveal(this._peek, this._factor) : super(null);

  double _peek, _factor;
  set peek(double value) {
    if (value == _peek) return;
    _peek = value;
    markNeedsLayout();
  }

  set factor(double value) {
    if (value == _factor) return;
    _factor = value;
    markNeedsLayout();
  }

  final _clip = LayerHandle<ClipRectLayer>();

  double _shown(double height) {
    final top = min(_peek, height);
    return top + (height - top) * _factor;
  }

  Size _sizeFor(BoxConstraints constraints, Size? child) =>
      constraints.constrain(
        Size(
          constraints.hasBoundedWidth
              ? constraints.maxWidth
              : child?.width ?? 0,
          child == null ? 0 : _shown(child.height),
        ),
      );

  @override
  double computeMinIntrinsicHeight(double width) =>
      _shown(super.computeMinIntrinsicHeight(width));

  @override
  double computeMaxIntrinsicHeight(double width) =>
      _shown(super.computeMaxIntrinsicHeight(width));

  @override
  Size computeDryLayout(covariant BoxConstraints constraints) =>
      _sizeFor(constraints, child?.getDryLayout(constraints.loosen()));

  @override
  void performLayout() {
    final child = this.child;
    child?.layout(constraints.loosen(), parentUsesSize: true);
    size = _sizeFor(constraints, child?.size);
    if (child != null) {
      (child.parentData! as BoxParentData).offset = Offset(
        (size.width - child.size.width) / 2,
        0,
      );
    }
  }

  @override
  void paint(PaintingContext context, Offset offset) {
    final child = this.child;
    if (child == null ||
        child.size.height <= size.height && child.size.width <= size.width) {
      _clip.layer = null;
      super.paint(context, offset);
      return;
    }
    _clip.layer = context.pushClipRect(
      needsCompositing,
      offset,
      Offset.zero & size,
      super.paint,
      oldLayer: _clip.layer,
    );
  }

  @override
  void dispose() {
    _clip.layer = null;
    super.dispose();
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
