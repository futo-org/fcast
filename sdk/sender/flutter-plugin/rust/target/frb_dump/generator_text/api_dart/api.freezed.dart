// GENERATED CODE - DO NOT MODIFY BY HAND
// coverage:ignore-file
// ignore_for_file: type=lint
// ignore_for_file: unused_element, deprecated_member_use, deprecated_member_use_from_same_package, use_function_type_syntax_for_parameters, unnecessary_const, avoid_init_to_null, invalid_override_different_default_values_named, prefer_expression_function_bodies, annotate_overrides, invalid_annotation_target, unnecessary_question_mark

part of 'api.dart';

// **************************************************************************
// FreezedGenerator
// **************************************************************************

// dart format off
T _$identity<T>(T value) => value;
/// @nodoc
mixin _$ErrorMessage {

 String get field0;
/// Create a copy of ErrorMessage
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$ErrorMessageCopyWith<ErrorMessage> get copyWith => _$ErrorMessageCopyWithImpl<ErrorMessage>(this as ErrorMessage, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is ErrorMessage&&(identical(other.field0, field0) || other.field0 == field0));
}


@override
int get hashCode => Object.hash(runtimeType,field0);

@override
String toString() {
  return 'ErrorMessage(field0: $field0)';
}


}

/// @nodoc
abstract mixin class $ErrorMessageCopyWith<$Res>  {
  factory $ErrorMessageCopyWith(ErrorMessage value, $Res Function(ErrorMessage) _then) = _$ErrorMessageCopyWithImpl;
@useResult
$Res call({
 String field0
});




}
/// @nodoc
class _$ErrorMessageCopyWithImpl<$Res>
    implements $ErrorMessageCopyWith<$Res> {
  _$ErrorMessageCopyWithImpl(this._self, this._then);

  final ErrorMessage _self;
  final $Res Function(ErrorMessage) _then;

/// Create a copy of ErrorMessage
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') @override $Res call({Object? field0 = null,}) {
  return _then(_self.copyWith(
field0: null == field0 ? _self.field0 : field0 // ignore: cast_nullable_to_non_nullable
as String,
  ));
}

}


/// Adds pattern-matching-related methods to [ErrorMessage].
extension ErrorMessagePatterns on ErrorMessage {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( ErrorMessage_Error value)?  error,required TResult orElse(),}){
final _that = this;
switch (_that) {
case ErrorMessage_Error() when error != null:
return error(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( ErrorMessage_Error value)  error,}){
final _that = this;
switch (_that) {
case ErrorMessage_Error():
return error(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( ErrorMessage_Error value)?  error,}){
final _that = this;
switch (_that) {
case ErrorMessage_Error() when error != null:
return error(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function( String field0)?  error,required TResult orElse(),}) {final _that = this;
switch (_that) {
case ErrorMessage_Error() when error != null:
return error(_that.field0);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function( String field0)  error,}) {final _that = this;
switch (_that) {
case ErrorMessage_Error():
return error(_that.field0);}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function( String field0)?  error,}) {final _that = this;
switch (_that) {
case ErrorMessage_Error() when error != null:
return error(_that.field0);case _:
  return null;

}
}

}

/// @nodoc


class ErrorMessage_Error extends ErrorMessage {
  const ErrorMessage_Error(this.field0): super._();
  

@override final  String field0;

/// Create a copy of ErrorMessage
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$ErrorMessage_ErrorCopyWith<ErrorMessage_Error> get copyWith => _$ErrorMessage_ErrorCopyWithImpl<ErrorMessage_Error>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is ErrorMessage_Error&&(identical(other.field0, field0) || other.field0 == field0));
}


@override
int get hashCode => Object.hash(runtimeType,field0);

@override
String toString() {
  return 'ErrorMessage.error(field0: $field0)';
}


}

/// @nodoc
abstract mixin class $ErrorMessage_ErrorCopyWith<$Res> implements $ErrorMessageCopyWith<$Res> {
  factory $ErrorMessage_ErrorCopyWith(ErrorMessage_Error value, $Res Function(ErrorMessage_Error) _then) = _$ErrorMessage_ErrorCopyWithImpl;
@override @useResult
$Res call({
 String field0
});




}
/// @nodoc
class _$ErrorMessage_ErrorCopyWithImpl<$Res>
    implements $ErrorMessage_ErrorCopyWith<$Res> {
  _$ErrorMessage_ErrorCopyWithImpl(this._self, this._then);

  final ErrorMessage_Error _self;
  final $Res Function(ErrorMessage_Error) _then;

/// Create a copy of ErrorMessage
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? field0 = null,}) {
  return _then(ErrorMessage_Error(
null == field0 ? _self.field0 : field0 // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc
mixin _$IpAddr {

 int get o1; int get o2; int get o3; int get o4;
/// Create a copy of IpAddr
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$IpAddrCopyWith<IpAddr> get copyWith => _$IpAddrCopyWithImpl<IpAddr>(this as IpAddr, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is IpAddr&&(identical(other.o1, o1) || other.o1 == o1)&&(identical(other.o2, o2) || other.o2 == o2)&&(identical(other.o3, o3) || other.o3 == o3)&&(identical(other.o4, o4) || other.o4 == o4));
}


@override
int get hashCode => Object.hash(runtimeType,o1,o2,o3,o4);

@override
String toString() {
  return 'IpAddr(o1: $o1, o2: $o2, o3: $o3, o4: $o4)';
}


}

/// @nodoc
abstract mixin class $IpAddrCopyWith<$Res>  {
  factory $IpAddrCopyWith(IpAddr value, $Res Function(IpAddr) _then) = _$IpAddrCopyWithImpl;
@useResult
$Res call({
 int o1, int o2, int o3, int o4
});




}
/// @nodoc
class _$IpAddrCopyWithImpl<$Res>
    implements $IpAddrCopyWith<$Res> {
  _$IpAddrCopyWithImpl(this._self, this._then);

  final IpAddr _self;
  final $Res Function(IpAddr) _then;

/// Create a copy of IpAddr
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') @override $Res call({Object? o1 = null,Object? o2 = null,Object? o3 = null,Object? o4 = null,}) {
  return _then(_self.copyWith(
o1: null == o1 ? _self.o1 : o1 // ignore: cast_nullable_to_non_nullable
as int,o2: null == o2 ? _self.o2 : o2 // ignore: cast_nullable_to_non_nullable
as int,o3: null == o3 ? _self.o3 : o3 // ignore: cast_nullable_to_non_nullable
as int,o4: null == o4 ? _self.o4 : o4 // ignore: cast_nullable_to_non_nullable
as int,
  ));
}

}


/// Adds pattern-matching-related methods to [IpAddr].
extension IpAddrPatterns on IpAddr {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( IpAddr_V4 value)?  v4,TResult Function( IpAddr_V6 value)?  v6,required TResult orElse(),}){
final _that = this;
switch (_that) {
case IpAddr_V4() when v4 != null:
return v4(_that);case IpAddr_V6() when v6 != null:
return v6(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( IpAddr_V4 value)  v4,required TResult Function( IpAddr_V6 value)  v6,}){
final _that = this;
switch (_that) {
case IpAddr_V4():
return v4(_that);case IpAddr_V6():
return v6(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( IpAddr_V4 value)?  v4,TResult? Function( IpAddr_V6 value)?  v6,}){
final _that = this;
switch (_that) {
case IpAddr_V4() when v4 != null:
return v4(_that);case IpAddr_V6() when v6 != null:
return v6(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function( int o1,  int o2,  int o3,  int o4)?  v4,TResult Function( int o1,  int o2,  int o3,  int o4,  int o5,  int o6,  int o7,  int o8,  int o9,  int o10,  int o11,  int o12,  int o13,  int o14,  int o15,  int o16,  int scopeId)?  v6,required TResult orElse(),}) {final _that = this;
switch (_that) {
case IpAddr_V4() when v4 != null:
return v4(_that.o1,_that.o2,_that.o3,_that.o4);case IpAddr_V6() when v6 != null:
return v6(_that.o1,_that.o2,_that.o3,_that.o4,_that.o5,_that.o6,_that.o7,_that.o8,_that.o9,_that.o10,_that.o11,_that.o12,_that.o13,_that.o14,_that.o15,_that.o16,_that.scopeId);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function( int o1,  int o2,  int o3,  int o4)  v4,required TResult Function( int o1,  int o2,  int o3,  int o4,  int o5,  int o6,  int o7,  int o8,  int o9,  int o10,  int o11,  int o12,  int o13,  int o14,  int o15,  int o16,  int scopeId)  v6,}) {final _that = this;
switch (_that) {
case IpAddr_V4():
return v4(_that.o1,_that.o2,_that.o3,_that.o4);case IpAddr_V6():
return v6(_that.o1,_that.o2,_that.o3,_that.o4,_that.o5,_that.o6,_that.o7,_that.o8,_that.o9,_that.o10,_that.o11,_that.o12,_that.o13,_that.o14,_that.o15,_that.o16,_that.scopeId);}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function( int o1,  int o2,  int o3,  int o4)?  v4,TResult? Function( int o1,  int o2,  int o3,  int o4,  int o5,  int o6,  int o7,  int o8,  int o9,  int o10,  int o11,  int o12,  int o13,  int o14,  int o15,  int o16,  int scopeId)?  v6,}) {final _that = this;
switch (_that) {
case IpAddr_V4() when v4 != null:
return v4(_that.o1,_that.o2,_that.o3,_that.o4);case IpAddr_V6() when v6 != null:
return v6(_that.o1,_that.o2,_that.o3,_that.o4,_that.o5,_that.o6,_that.o7,_that.o8,_that.o9,_that.o10,_that.o11,_that.o12,_that.o13,_that.o14,_that.o15,_that.o16,_that.scopeId);case _:
  return null;

}
}

}

/// @nodoc


class IpAddr_V4 extends IpAddr {
  const IpAddr_V4({required this.o1, required this.o2, required this.o3, required this.o4}): super._();
  

@override final  int o1;
@override final  int o2;
@override final  int o3;
@override final  int o4;

/// Create a copy of IpAddr
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$IpAddr_V4CopyWith<IpAddr_V4> get copyWith => _$IpAddr_V4CopyWithImpl<IpAddr_V4>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is IpAddr_V4&&(identical(other.o1, o1) || other.o1 == o1)&&(identical(other.o2, o2) || other.o2 == o2)&&(identical(other.o3, o3) || other.o3 == o3)&&(identical(other.o4, o4) || other.o4 == o4));
}


@override
int get hashCode => Object.hash(runtimeType,o1,o2,o3,o4);

@override
String toString() {
  return 'IpAddr.v4(o1: $o1, o2: $o2, o3: $o3, o4: $o4)';
}


}

/// @nodoc
abstract mixin class $IpAddr_V4CopyWith<$Res> implements $IpAddrCopyWith<$Res> {
  factory $IpAddr_V4CopyWith(IpAddr_V4 value, $Res Function(IpAddr_V4) _then) = _$IpAddr_V4CopyWithImpl;
@override @useResult
$Res call({
 int o1, int o2, int o3, int o4
});




}
/// @nodoc
class _$IpAddr_V4CopyWithImpl<$Res>
    implements $IpAddr_V4CopyWith<$Res> {
  _$IpAddr_V4CopyWithImpl(this._self, this._then);

  final IpAddr_V4 _self;
  final $Res Function(IpAddr_V4) _then;

/// Create a copy of IpAddr
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? o1 = null,Object? o2 = null,Object? o3 = null,Object? o4 = null,}) {
  return _then(IpAddr_V4(
o1: null == o1 ? _self.o1 : o1 // ignore: cast_nullable_to_non_nullable
as int,o2: null == o2 ? _self.o2 : o2 // ignore: cast_nullable_to_non_nullable
as int,o3: null == o3 ? _self.o3 : o3 // ignore: cast_nullable_to_non_nullable
as int,o4: null == o4 ? _self.o4 : o4 // ignore: cast_nullable_to_non_nullable
as int,
  ));
}


}

/// @nodoc


class IpAddr_V6 extends IpAddr {
  const IpAddr_V6({required this.o1, required this.o2, required this.o3, required this.o4, required this.o5, required this.o6, required this.o7, required this.o8, required this.o9, required this.o10, required this.o11, required this.o12, required this.o13, required this.o14, required this.o15, required this.o16, required this.scopeId}): super._();
  

@override final  int o1;
@override final  int o2;
@override final  int o3;
@override final  int o4;
 final  int o5;
 final  int o6;
 final  int o7;
 final  int o8;
 final  int o9;
 final  int o10;
 final  int o11;
 final  int o12;
 final  int o13;
 final  int o14;
 final  int o15;
 final  int o16;
 final  int scopeId;

/// Create a copy of IpAddr
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$IpAddr_V6CopyWith<IpAddr_V6> get copyWith => _$IpAddr_V6CopyWithImpl<IpAddr_V6>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is IpAddr_V6&&(identical(other.o1, o1) || other.o1 == o1)&&(identical(other.o2, o2) || other.o2 == o2)&&(identical(other.o3, o3) || other.o3 == o3)&&(identical(other.o4, o4) || other.o4 == o4)&&(identical(other.o5, o5) || other.o5 == o5)&&(identical(other.o6, o6) || other.o6 == o6)&&(identical(other.o7, o7) || other.o7 == o7)&&(identical(other.o8, o8) || other.o8 == o8)&&(identical(other.o9, o9) || other.o9 == o9)&&(identical(other.o10, o10) || other.o10 == o10)&&(identical(other.o11, o11) || other.o11 == o11)&&(identical(other.o12, o12) || other.o12 == o12)&&(identical(other.o13, o13) || other.o13 == o13)&&(identical(other.o14, o14) || other.o14 == o14)&&(identical(other.o15, o15) || other.o15 == o15)&&(identical(other.o16, o16) || other.o16 == o16)&&(identical(other.scopeId, scopeId) || other.scopeId == scopeId));
}


@override
int get hashCode => Object.hash(runtimeType,o1,o2,o3,o4,o5,o6,o7,o8,o9,o10,o11,o12,o13,o14,o15,o16,scopeId);

@override
String toString() {
  return 'IpAddr.v6(o1: $o1, o2: $o2, o3: $o3, o4: $o4, o5: $o5, o6: $o6, o7: $o7, o8: $o8, o9: $o9, o10: $o10, o11: $o11, o12: $o12, o13: $o13, o14: $o14, o15: $o15, o16: $o16, scopeId: $scopeId)';
}


}

/// @nodoc
abstract mixin class $IpAddr_V6CopyWith<$Res> implements $IpAddrCopyWith<$Res> {
  factory $IpAddr_V6CopyWith(IpAddr_V6 value, $Res Function(IpAddr_V6) _then) = _$IpAddr_V6CopyWithImpl;
@override @useResult
$Res call({
 int o1, int o2, int o3, int o4, int o5, int o6, int o7, int o8, int o9, int o10, int o11, int o12, int o13, int o14, int o15, int o16, int scopeId
});




}
/// @nodoc
class _$IpAddr_V6CopyWithImpl<$Res>
    implements $IpAddr_V6CopyWith<$Res> {
  _$IpAddr_V6CopyWithImpl(this._self, this._then);

  final IpAddr_V6 _self;
  final $Res Function(IpAddr_V6) _then;

/// Create a copy of IpAddr
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? o1 = null,Object? o2 = null,Object? o3 = null,Object? o4 = null,Object? o5 = null,Object? o6 = null,Object? o7 = null,Object? o8 = null,Object? o9 = null,Object? o10 = null,Object? o11 = null,Object? o12 = null,Object? o13 = null,Object? o14 = null,Object? o15 = null,Object? o16 = null,Object? scopeId = null,}) {
  return _then(IpAddr_V6(
o1: null == o1 ? _self.o1 : o1 // ignore: cast_nullable_to_non_nullable
as int,o2: null == o2 ? _self.o2 : o2 // ignore: cast_nullable_to_non_nullable
as int,o3: null == o3 ? _self.o3 : o3 // ignore: cast_nullable_to_non_nullable
as int,o4: null == o4 ? _self.o4 : o4 // ignore: cast_nullable_to_non_nullable
as int,o5: null == o5 ? _self.o5 : o5 // ignore: cast_nullable_to_non_nullable
as int,o6: null == o6 ? _self.o6 : o6 // ignore: cast_nullable_to_non_nullable
as int,o7: null == o7 ? _self.o7 : o7 // ignore: cast_nullable_to_non_nullable
as int,o8: null == o8 ? _self.o8 : o8 // ignore: cast_nullable_to_non_nullable
as int,o9: null == o9 ? _self.o9 : o9 // ignore: cast_nullable_to_non_nullable
as int,o10: null == o10 ? _self.o10 : o10 // ignore: cast_nullable_to_non_nullable
as int,o11: null == o11 ? _self.o11 : o11 // ignore: cast_nullable_to_non_nullable
as int,o12: null == o12 ? _self.o12 : o12 // ignore: cast_nullable_to_non_nullable
as int,o13: null == o13 ? _self.o13 : o13 // ignore: cast_nullable_to_non_nullable
as int,o14: null == o14 ? _self.o14 : o14 // ignore: cast_nullable_to_non_nullable
as int,o15: null == o15 ? _self.o15 : o15 // ignore: cast_nullable_to_non_nullable
as int,o16: null == o16 ? _self.o16 : o16 // ignore: cast_nullable_to_non_nullable
as int,scopeId: null == scopeId ? _self.scopeId : scopeId // ignore: cast_nullable_to_non_nullable
as int,
  ));
}


}

// dart format on
